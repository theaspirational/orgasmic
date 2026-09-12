# Review TASK-1PFW6 — Marketplace registry in core, daemon and CLI

## Verdict

**Request changes.** No blocking finding: the trust boundary (brief item 1) and
authz (item 2) both hold up under inspection — `:SOURCE:` cannot escape the
clone, symlinks are refused on both the read and the copy side, `--` precedes
every git positional, and every mutation is admin-only twice over. The changes
requested are three real user-facing bugs (M1, M2, M3) plus a contract gap (M4)
and a test gap (M5).

## Findings

### M1 — MEDIUM (bug, usability): `orgasmic marketplace add` times out client-side at 10s while the daemon keeps cloning

`crates/orgasmic-cli/src/daemon_client.rs:53` sets `DEFAULT_REQUEST_TIMEOUT_SECS
= 10` and `:77` applies it as a client-wide `reqwest` timeout.
`crates/orgasmic-cli/src/marketplace.rs:43` (`add`) and `:60` (`refresh`) use the
plain `post_json`, so they inherit it.

A shallow clone of the shipped official repo
(`github.com/theaspirational/orgasmic-plugins`, `shipped/marketplaces.org:7`)
over a real network routinely exceeds 10s, and `orgasmic marketplace refresh`
with no key pulls **every** marketplace sequentially
(`crates/orgasmic-daemon/src/marketplaces.rs:139-165`).

Symptom: the CLI prints a transport/timeout error, the daemon completes the add
anyway, and the obvious retry answers `marketplace is already registered`
(`marketplaces.rs:107-110`). The user is left believing the command failed while
it succeeded. This is the primary happy path for the only shipped marketplace.

Fix: a per-request timeout override, exactly as this file already does twice —
`post_full_board_json` (`daemon_client.rs:125`) and `post_dispatch` (`:162`).
Add a `MARKETPLACE_REQUEST_TIMEOUT_SECS` and route add/refresh/install/update
through a `.timeout(...)` variant.

### M2 — MEDIUM (bug, availability): blocking `git` on the tokio runtime with no timeout; `plugin install` holds `plugins.operations.write()` across it

`crates/orgasmic-daemon/src/marketplaces.rs:517` — `fn git` is
`std::process::Command::output()`, i.e. fully synchronous. Its callers run inside
async axum handlers: `add` (`:111`) from `api.rs:15875`, `refresh` (`:144`) from
`api.rs:15886`, and `stage_plugin` (`:388`) from `api.rs:15959`. None of them
uses `spawn_blocking`, and git has no default network timeout, so an unreachable
remote holds a tokio worker thread indefinitely.

The sharp case is install. `post_plugin_install` (`api.rs:15965`) takes
`state.plugins.operations.write().await` **before** calling
`marketplaces.install`. When the entry's `:SOURCE:` is a git URL,
`stage_plugin` (`marketplaces.rs:386-395`) performs a network clone under that
write guard. `get_plugin_ui_asset` (`api.rs:1239`) takes the matching read guard
and tokio's `RwLock` is write-preferring, so for the whole duration of that clone
every plugin UI asset request blocks, and the 1s `reconcile_if_changed` loop
(`plugins.rs:152-163`) blocks with it. A hung remote wedges the plugin subsystem
daemon-wide, not just the one request.

The `api.rs` sync-`Command::new("git")` precedent (e.g. `:2403`, `:7331`,
`:7588`) is all *local* repo work — status, rev-parse, commit — which returns in
milliseconds. It does not cover a network clone, so it does not license this.

Fix: wrap `fn git` in `tokio::task::spawn_blocking`, and bound it —
either spawn/`try_wait`/kill after N seconds, or set
`GIT_HTTP_LOW_SPEED_LIMIT` / `GIT_HTTP_LOW_SPEED_TIME` on the child. Also move
the clone outside the `plugins.operations` write guard: stage first, take the
guard only for the rename + reconcile.

### M3 — MEDIUM (bug): one broken marketplace index 400s `GET /marketplaces/plugins` for every marketplace

`crates/orgasmic-daemon/src/marketplaces.rs:220` —
`let index = MarketplaceIndex::read_dir(&root)?;` sits inside the per-record
loop, so a single unparseable `marketplace.org` aborts the whole listing.
Contrast `list()` (`:76-81`), which deliberately degrades per entry and reports
the parse error in the `error` field. The browse route therefore fails
wholesale where the list route does not.

It is reachable from untrusted input: `refresh` (`:143-153`) runs
`git pull --ff-only` and validates the index *afterwards*. On a validation
failure it records the error and returns `Err` — but the working tree is already
at the bad commit. A marketplace (including the official one) that publishes one
malformed `marketplace.org` kills plugin browsing daemon-wide until an admin
manually removes it.

Fix: collect per-marketplace errors in `plugins()` instead of `?`, mirroring
`list()`. Optionally also capture the pre-pull `HEAD` and reset to it when the
post-pull `read_dir` fails, so a bad publish does not stick.

### M4 — MEDIUM (correctness, contract drift): git-URL `:SOURCE:` entries report the *installed* version and capabilities as the offered ones

`crates/orgasmic-daemon/src/marketplaces.rs:316-318` —
`if is_git_source(&entry.source) { return Ok(installed.cloned()); }`.

Consequences for a git-sourced entry: `version` echoes the installed version
rather than the offered one; `update_available` is therefore always `false`
(`:225-231`); and before install `capabilities` is empty (`:242-244`), so the
admin approves blind.

`PLUGINS-SCOPE.md` §12 states "Version and capabilities come from the plugin's
own `plugin.org`, never from the index." For a git `:SOURCE:` this branch never
reads that `plugin.org` at all. `orgasmic plugin update <id>` still fetches the
new version correctly, but nothing in the UI or the browse route ever signals
that an update exists.

No test covers a git-URL `:SOURCE:`: both fixtures in
`tests/marketplace_routes.rs:38,40` use the relative form `plugins/meetings` /
`plugins/tasks-plus`, so this path is entirely unexercised.

Fix: shallow-clone git sources into a refresh-time cache under the marketplace
root and read the manifest from there; or, if that is deliberately deferred,
say so in §12 and return `version: null` with an explicit `unknown` rather than
echoing the installed manifest, so the UI cannot present stale data as offered.

### M5 — MEDIUM (test): non-admin refusal is exercised for 2 of 7 mutations

`tests/marketplace_routes.rs:171-180` covers `/marketplaces/:key/activation` and
`:203-212` covers `/plugins/install`. Nothing probes a member token against
`POST /marketplaces`, `POST /marketplaces/refresh`,
`POST /marketplaces/:key/refresh`, `POST /marketplaces/:key/remove`, or
`POST /plugins/:id/update`.

The code is correct — each handler calls
`authz::require(&identity, None, Action::MembersManage)` (`api.rs:15882, 15893,
15903, 15914, 15931, 15964, 15989`), and `MEMBER_ALLOWED_ROUTES` (`api.rs:965`)
omits all five so the middleware fails closed first (`api.rs:955`: "everything
NOT listed here is implicitly admin-only"). I verified the fail-closed path
directly: for a `Member` identity `require(.., None, ..)` returns
`Forbidden("action requires a project")` (`authz.rs:266-268`), and for a
`Plugin` identity it returns before that (`authz.rs:232-234`).

But nothing *pins* either layer. Dropping a `require` line leaves the table as
the only defence; adding a route to `MEMBER_ALLOWED_ROUTES` leaves the handler
as the only defence. Neither regression would go red today.

Fix: loop the existing member token over all five routes in one assertion block
next to `:171`.

### M6 — LOW (bug, reuse): CLI reimplements `path_segment` and under-encodes

`crates/orgasmic-cli/src/marketplace.rs:83` —
`value.replace('%', "%25").replace('/', "%2F")`.
`crates/orgasmic-cli/src/daemon_client.rs:321` already has a `pub(crate) fn
path_segment` that percent-encodes everything outside the unreserved set, and it
is already used by `post_dispatch` (`:163`).

Marketplace keys can contain `?` and `#`: `marketplace_key`
(`crates/orgasmic-core/src/marketplace.rs:139`) only strips the scheme, a
trailing `.git` and slashes, and `validate_relative_source` (`:164`) only rejects
`.`, `..` and absolute paths. So `https://host/a/b?ref=x` yields the key
`host/a/b?ref=x`, which the daemon then creates as a directory.
`orgasmic marketplace remove 'host/a/b?ref=x'` sends
`/marketplaces/host%2Fa%2Fb?ref=x/remove` — the tail becomes a query string and
the request misses the route. `#` truncates the path outright.

Fix: delete the duplicate, `use crate::daemon_client::path_segment;`.

### L1 — LOW (cleanup): removed marketplace clones are never deleted

`marketplaces.rs:177-182` renames the clone into
`~/.orgasmic/user/marketplaces-removed/<uuid>` and nothing ever reaps it. Every
add/remove cycle leaks a full shallow clone into the user home forever. Fix:
`remove_dir_all` after the record write succeeds, or reap the directory on daemon
start.

### L2 — LOW (correctness): `refresh` with no key aborts on the first failure

`marketplaces.rs:154-164` returns `Err` from inside the loop, so marketplaces
after the failing one are never pulled and the caller learns nothing about which
ones succeeded. Fix: record per-key errors (the `state.errors` map already
exists) and return `self.list()` as the success path does.

### L3 — LOW (usability): a typo'd local path is routed to the marketplace installer

`crates/orgasmic-cli/src/plugin.rs:131-133` —
`!Path::new(&source).exists() && !source.contains("://") && !source.starts_with("git@")`.
`orgasmic plugin add ./meetigns` becomes `marketplace: "."`, `id: "meetigns"`
and the user gets `unknown marketplace` instead of "no such directory". Fix:
treat a source starting with `./`, `../`, `~` or `/` as a path and report the
missing directory.

### L4 — LOW (hardening): `:SOURCE:` accepts `file://`

`crates/orgasmic-core/src/marketplace.rs:135` — `is_git_source` allows
`file://` for plugin sources as well as marketplace URLs. A remote, untrusted
marketplace index can therefore point a plugin at a local git repo on the daemon
host and have it copied into `~/.orgasmic/user/plugins/<id>/`.

Answering the brief's question directly: this is **not** a hole. The target must
be a real git repo whose `plugin.org` parses and whose `:ID:` equals the entry
id (`marketplaces.rs:262-266`), the result is installed disabled, and UI assets
are served only for an enabled plugin and only as `.js`/`.css` under `ui/`
(`api.rs:1247-1273`). And `marketplace add` is admin-only — an admin runs as the
daemon's own OS user and could write `~/.orgasmic/user/plugins` directly, so
`file://` **marketplace URLs** grant nothing new even in production. It is needed
for the no-network tests (`tests/marketplace_routes.rs:73`).

Fix: keep `file://` for marketplace URLs, reject it for `:SOURCE:` — the
marketplace repo is the untrusted party there, not the admin. One line in §12
noting that `file://` marketplace URLs are a test affordance would also help.

### L5 — LOW (hardening): `.orgasmic-install.org` is attacker-supplied on the direct-add path

`marketplaces.rs:539` writes the install metadata after staging, so a marketplace
repo cannot forge it — correct. But `crates/orgasmic-cli/src/plugin.rs:150`
(`plugin add <git-url>`) copies the tree with `copy_plugin_tree` and never writes
or strips that file. A repo added directly by URL can ship an
`.orgasmic-install.org` naming a registered marketplace, and a later
`orgasmic plugin update <id>` (`marketplaces.rs:278`) would silently replace it
from there. Fix: have `copy_plugin_tree`
(`crates/orgasmic-core/src/plugin.rs:307`) skip `.orgasmic-install.org` the way
it skips `.git`.

### L6 — LOW (usability): `install` does not pre-check the core-reserved id

`marketplaces.rs:257-258` only checks `!destination.exists()`.
`plugins.rs:242-245` rejects an id colliding with a core descriptor
("plugin id is reserved by core"), but only at reconcile time — so the install
succeeds, the folder lands in `~/.orgasmic/user/plugins/<id>`, and the plugin
shows a permanent error the admin must clean up by hand. Fix: run the same
`base.descriptor(&id).is_none()` check before the rename.

(There is no `shipped/plugins/` directory — `ls shipped/` gives `entry`,
`marketplaces.org`, `project-scaffold`, `prompt-studio`, `schema`, `skills`,
`workflows` — so a shipped-built-in folder collision, which the brief asked
about, cannot arise by any route other than this core-descriptor one.)

### L7 — LOW (correctness): the key is case-sensitive, the filesystem may not be

`marketplace_key` (`crates/orgasmic-core/src/marketplace.rs:139`) never
lowercases the host, so `https://GitHub.com/a/b` and `https://github.com/a/b`
are two distinct records. On Linux that is two clones of one repo and two list
entries; on macOS APFS they collide on disk and the second `add` fails with the
misleading `marketplace is already cloned` (`marketplaces.rs:362`). Fix:
lowercase the host component in `marketplace_key`.

## What holds up (checked, no finding)

- **Path traversal / symlink escape (item 1).** `MarketplacePlugin::source_dir`
  (`core/marketplace.rs:109-131`) walks each component rejecting non-`Normal`
  components, `lstat`s every component for a symlink, then canonicalizes and
  asserts `starts_with(root)` and `is_dir`. `MarketplaceIndex::read_dir`
  (`:88-97`) separately refuses a symlinked marketplace root and a symlinked
  `marketplace.org`. `copy_plugin_tree` (`core/plugin.rs:307-327`) uses
  `entry.file_type()` from `read_dir`, which is `lstat`-based, so a symlink is
  neither `is_dir()` nor `is_file()` and hits the `bail!` — it is never followed
  and never copied. `.git` is skipped at every level. The unit test at
  `core/marketplace.rs:198-215` covers `..`, absolute and embedded-`..`; the test
  at `:218-229` covers a symlinked source folder.
- **Argument injection.** `--` precedes every positional in all three git
  invocations: `marketplaces.rs:372`, `marketplaces.rs:391`,
  `cli/plugin.rs:150`. `git pull` uses `-C <absolute path>`, and the path is
  always rooted at `home.marketplaces()`, so it can never start with `-`.
  `validate_url` (`marketplaces.rs:509`) also rejects whitespace.
- **Installed folder name.** `install` (`:253, 257, 263-266`) validates the id,
  refuses an existing destination, and asserts `manifest.id == id` after staging,
  so the folder name is the manifest id.
- **Officials are disable-only and cannot be demoted.** `records()`
  (`:418-423`) overwrites `official` from the shipped table for every user
  record, then merges in any official the user file omits — so a hand-written
  user entry for an official key stays official. `remove` (`:173-176`) refuses.
  The test exercises exactly that sequence: disable the official (which writes
  the user file containing the official key, `:196-206`), then remove → 400
  (`tests:353-387`).
- **Update semantics (item 3).** The swap is stage → validate → write metadata →
  `rename(dest, backup)` → `rename(staged, dest)` with a rollback on failure
  (`:284-300`); no half-state. Per-project activation survives — the test asserts
  `.orgasmic/plugins.json` still has `enabled: true` after the swap
  (`tests:308-311`). Grown capabilities go through the existing gate:
  `plugins.rs:315-324` refuses to activate when
  `!manifest.capabilities.is_subset(&activation.approved_capabilities)` and
  surfaces "capabilities grew; enable again to approve them", asserted at
  `tests:321-328`. A running UI view is protected: both install and update hold
  `plugins.operations.write()` (`api.rs:15965, 15990`) and
  `get_plugin_ui_asset` holds the matching read guard (`api.rs:1239`), so no
  request is served from the rename gap. (The cost of that correct choice is M2.)
- **Key derivation (item 5).** `https://` and `git@` collapse to the same key —
  `marketplace_key` (`:139-162`) strips the scheme, converts `git@host:path` to
  `host/path`, strips `.git` and trailing slashes; unit-tested at `:208-217` and
  asserted against the route response at `tests:156`. A second `add` of the same
  repo under either form hits `marketplace is already registered` (`:107-110`),
  so no duplicate clone. The CLI does URL-encode the key (`marketplace.rs:83`) —
  see M6 for the encoding's gap, not its absence.
- **Shipped list (item 6).** `shipped/marketplaces.org` holds exactly one entry,
  the GitHub official.
- **CLI (item 7).** `plugin add <path|git-url>` is unchanged apart from the
  `copy_tree` → `copy_plugin_tree` move (byte-identical body, verified in the
  diff). `plugin add <marketplace>/<id>` uses `rsplit_once('/')`
  (`cli/plugin.rs:135`), which correctly handles both a multi-segment key
  (`github.com/theaspirational/orgasmic-plugins/meetings`) and an alias
  (`local-market/meetings`); `resolve_key` (`marketplaces.rs:341-358`) tries the
  key first, then aliases, and refuses an ambiguous alias. Parity is covered
  indirectly but genuinely: `SKILL.md` now names all six `marketplace` verbs and
  `plugin update`, and `cli_parity.rs` gates shipped prose against clap leaves —
  so a missing verb would go red there. No *behavioral* CLI test exists (see
  Verification Notes).

## Open Questions

1. M4: is the git-URL `:SOURCE:` path intended to ship in this task, or deferred?
   It has no test and no working version/capability display. If deferred, I would
   rather see `is_git_source` reject it for `:SOURCE:` (which also closes L4)
   than ship a browse view that shows stale data as offered.
2. L1: is `marketplaces-removed/` a deliberate undo affordance? If so it needs a
   reaper and a CLI verb; if not it should just be deleted.
3. §12 says "a rewritten history is reported as an error, and the user removes
   and re-adds". `git pull --ff-only` does produce an error, and `refresh` stores
   it (`:157-161`), but nothing distinguishes it from a network failure, so the
   user gets no hint to remove-and-re-add. Intentional for now?

## Verification Notes

Worktree `/Users/aspirational/.orgasmic/worktrees/orgasmic/task-1pfw6-review`,
branch `task-1pfw6-review`, single commit `5c7a8ad0` vs `main`. Worktree-local
`CARGO_TARGET_DIR=.../task-1pfw6-review/target-review`. No daemon started. Logs
under `/tmp/1pfw6-logs/`.

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | rc=0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | rc=0 |
| `cargo test -p orgasmic-core` | rc=0 |
| `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes` | rc=0, 2 passed |
| `cargo test -p orgasmic-cli -- --test-threads=1` | rc=101, **380 passed, 1 failed** |

**The one CLI failure is environment-blocked, not a regression.**
`manager::tests::empty_private_targets_never_run_another_worktrees_binary`
panicked at `crates/orgasmic-cli/src/manager.rs:12939` with
`Os { code: 2, kind: NotFound }` while trying to execute
`<worktree>/target/debug/private-target-probe`. The test spawns two child
`cargo build` runs (`manager.rs:12909-12923`) and never clears `CARGO_TARGET_DIR`
— I grepped the whole test body for `CARGO_TARGET_DIR`/`env_remove` and found
neither — so the children inherited the worktree-local value the brief told me to
set and wrote the probe binary outside the directory the test then reads.
Proven, not inferred: `ls target-review/debug/private-target-probe` finds the
binary sitting in my shared target dir. `manager.rs` is untouched by this diff
(`git diff main...HEAD --stat` lists 16 files, none of them `manager.rs`).

**Production-path probes.** The daemon route test drives the real HTTP surface
against local bare git repos with `file://` remotes and no network
(`tests/marketplace_routes.rs:63-77`), covering add → member-refusal → install →
duplicate-install → member-refusal → activate → publish → refresh → browse →
update → publish-with-grown-caps → refresh → update → capability re-approval →
remove → recommended → official disable → official remove-refusal. That is a
genuine end-to-end path, not a unit stub; I did not need to add one.

**Not independently re-run.** The brief scoped the gate to the five commands
above; I did not run the full workspace suite.

**Residual risk.** M2's blocking/timeout behaviour and M1's 10s client timeout
are both network-dependent and cannot be shown by the local-`file://` test
design, which completes in milliseconds. They are established from source
(`daemon_client.rs:53,77,121`; `marketplaces.rs:517`; `api.rs:15965`;
`plugins.rs:152`) rather than from a reproduction. M4's git-URL `:SOURCE:` path
has no test anywhere, so its runtime behaviour beyond the early return at
`marketplaces.rs:316` is unverified by anyone, including the implementer.

## Fix Directions

Must fix before merge:

1. **M1** — per-request timeout override on the four marketplace CLI calls,
   following `post_full_board_json` (`daemon_client.rs:125`).
2. **M2** — `spawn_blocking` around `marketplaces.rs:517` `fn git` plus a hard
   timeout on the child; move the install-time clone outside the
   `plugins.operations` write guard.
3. **M3** — per-marketplace error collection in `plugins()`
   (`marketplaces.rs:220`) instead of `?`.

Should fix:

4. **M4** — either read the offered manifest for git sources, or reject
   `file://`/git `:SOURCE:` for now and say so in §12.
5. **M5** — one loop asserting 403 for a member across all five untested
   mutations.
6. **M6** — delete `cli/marketplace.rs:83`, import
   `daemon_client::path_segment`.

Nice to have: L1–L7 as listed, each a one- to three-line change.

One stale annotation worth sweeping while in there: `Action::MembersManage`
still carries `#[allow(dead_code)]` (`authz.rs:39`) and the doc comment above it
(`:13-15`) still says it "has no v1 HTTP route" — this branch gives it seven.
