# Review TASK-N4QMC — Meetings moved out, fixture plugin, decision 9 reversed

## Verdict

**APPROVE.** All three acceptance criteria are met and every assigned test
suite passes. No production code changed. Four findings, none blocking — the
top two are test-coverage debt a follow-up task should pick up, not defects in
this diff.

## Findings

### MEDIUM (test) — the Meetings plugin's JavaScript now has zero tests in either repo

`ui/src/components/__tests__/MeetingsPlayer.test.jsx` (deleted, 8 `it()`
blocks) → `ui/src/components/__tests__/PluginFixture.test.jsx` (added, 1
`it()` block). The plugin code it covered moved to
`github.com/theaspirational/orgasmic-plugins`, but the test file did not. I
listed that repo's tree:

    README.md
    marketplace.org
    plugins/meetings/{README.md,plugin.org,bin/import,prompts/meeting-chat.org,ui/index.js,ui/player.js}

No test files. So 318 lines of plugin JS (`ui/index.js` 180 + `ui/player.js`
138) are now untested everywhere.

Most of the *wire contract* those tests rode on is still covered host-side, so
this is debt, not a hole:

| Deleted assertion | Host-side cover that survives |
|---|---|
| `/links` anchors + `base_revision: 0` | `crates/orgasmic-daemon/tests/node_services_routes.rs:448,463` |
| chip shapes `range`/`attachment`/`selection` | `ui/src/lib/__tests__/conversations.test.ts:75-91`, `ui/src/lib/__tests__/pluginRuntimeChat.test.tsx:19`, `ui/src/components/manager/__tests__/ConversationPanel.test.tsx:80` |
| 4 KiB selection cap | `crates/orgasmic-daemon/src/conversations.rs:28,1835` + test at `:2313` |
| range `end_ms > start_ms` | `crates/orgasmic-daemon/src/conversations.rs:1829` |

Two pieces of **client-side** logic are now untested anywhere, and the host
cannot substitute for them because the host only rejects malformed input — it
never proves the plugin *produces* well-formed input:

1. **The 30 s range window clamped to recording duration.** Deleted tests
   `opens the meeting chat with a 30 s range chip at the playhead, clamped to
   the recording` and `...without a range chip when the playhead is at the
   end`. The host only checks `end_ms > start_ms`; nothing proves the window is
   30 s, nor that a playhead at the end emits no chip rather than a zero-length
   one.
2. **UTF-8-safe truncation of the selection at 4 KiB.** Deleted test `chats
   about the selected notes text, capped at 4 KiB on a character boundary`
   asserted `text.includes('�') === false` and `text.endsWith('…')`. The
   host's cap is a byte-length *rejection*; it does not verify the client
   truncates on a character boundary. A regression here turns into a 400 or a
   replacement character, and no test catches it.

Fix direction: port `MeetingsPlayer.test.jsx` into the marketplace repo
alongside the plugin, or keep a trimmed copy of just items 1 and 2 here
against the marketplace checkout. Worth its own task.

### MEDIUM (docs) — the host's own ignored 2 GiB gate lost its documented invocation

`crates/orgasmic-daemon/tests/node_services_routes.rs:1078` is
`#[ignore = "2 GiB streaming disk/network gate; run explicitly"]`. The only
place in this repo that recorded how to run it was the deleted
`examples/plugins/meetings/README.md:55`:

    RUST_TEST_THREADS=1 cargo test -p orgasmic-daemon --test node_services_routes two_gib_recording_upload_and_seek -- --ignored

After this diff, `rg two_gib_recording_upload_and_seek` in this repo matches
only the function definition. The instruction now lives solely in the external
marketplace repo's plugin README — and that README is already stale: line 56
still tells the reader to run
`src/components/__tests__/MeetingsPlayer.test.jsx`, which exists in neither
repo.

A host test's run instruction should not live in a third-party plugin repo.

Fix direction: move that one command into this repo — a doc comment above
`node_services_routes.rs:1078`, or the release/verification doc that lists the
other explicit gates. Separately, the marketplace README's Verification block
needs its `MeetingsPlayer.test.jsx` line dropped.

### LOW (docs) — section 12 omits `marketplace.org`, the file that defines a marketplace

`PLUGINS-SCOPE.md:362-371` is presented as the marketplace contract, and it
matches the brief's checklist item by item (see Verification Notes). But it
never mentions `marketplace.org` — the index that decision 9
(`PLUGINS-SCOPE.md:338`) says *is* what makes a git repo a marketplace, and
that the real repo ships:

    * Marketplace
    :PROPERTIES:
    :ID: orgasmic-plugins
    :NAME: Orgasmic official plugins
    :DESCRIPTION: Plugins maintained by the Orgasmic project.
    :END:
    ** Plugin meetings
    :PROPERTIES:
    :ID: meetings
    :SOURCE: plugins/meetings
    :DESCRIPTION: ...
    :END:

Someone implementing `orgasmic marketplace add` from section 12 alone would
not know to read `marketplace.org`, nor its schema, nor that `:SOURCE:` is
what `plugin add <marketplace>/<id>` resolves. Since no code implements this
yet, the spec is the only source of truth.

Also on `PLUGINS-SCOPE.md:364-365`: "shallow clones" plus "refresh with
`git pull`". That combination works in current git but breaks on a
force-pushed or rewritten marketplace history. Worth one clause saying which
wins, or dropping "shallow".

Fix direction: add the `marketplace.org` schema and the `:SOURCE:` resolution
rule to section 12.

### LOW (docs) — stale comment now self-contradictory

`crates/orgasmic-daemon/tests/conversations_routes.rs:186-187`:

    // The shipped example predates chat.*; this fixture's plugin
    // asks for both so the conversation visibility gate is exercised.

The wording is pre-existing (unchanged context in the diff), but "the shipped
example" no longer exists in this repo, and the sentence now calls the same
file both "the shipped example" and "this fixture's plugin". The `replacen`
below it still works — I confirmed the fixture's `:CAPABILITIES:` line carries
no `chat.*`, so the insertion and its `assert!(manifest.contains("chat.read"))`
both hold. Comment only.

## Open Questions

1. Should the deleted `MeetingsPlayer.test.jsx` be ported to the marketplace
   repo, or should this repo keep a trimmed copy pinned to a marketplace
   checkout? Finding 1 needs that call before a follow-up task can be scoped.
2. Does `git.ath/orgasmic/plugins` (the internal edge, per decision 9) carry
   the same Meetings content? I verified only the GitHub remote; I have no
   read path to `git.ath` from this worktree.
3. Section 12 says install/update are admin-only. Is `marketplace list`
   admin-only too? The bullet at `PLUGINS-SCOPE.md:368` lists "add, remove,
   refresh, install, or update" but not `list`, while the CLI bullet at `:369`
   includes `list`. Probably deliberate; worth confirming.

## Verification Notes

Environment: worktree `/Users/aspirational/.orgasmic/worktrees/orgasmic/task-n4qmc-review`,
branch `task-n4qmc-review`, single commit `a431f490` against `main`.
`CARGO_TARGET_DIR=<worktree>/target-review` (worktree-local, not shared — a
sibling target dir would have produced a false green). No daemon started.
Logs: `/tmp/n4qmc-review/cargo.log`, `/tmp/n4qmc-review/ui.log`.

**Check 1 — no production code changed: CONFIRMED.** The diff touches
`crates/orgasmic-core/src/plugin.rs`, which looks like production but is not:
the whole hunk is inside `mod tests` (`plugin.rs:412-425`), renaming
`the_example_meetings_plugin_declares_chat_and_ships_its_prompt` →
`the_fixture_plugin_declares_chat_and_ships_its_prompt` and repointing the
path. Every other changed file is a test, a fixture, or a doc.

**Check 2 — `rg examples/plugins` empty: CONFIRMED.**

    rg -n "examples/plugins" --glob '!node_modules' --glob '!target' .
    → exit 1, no matches

I also ran `rg -n "player\.js|ui/player"` across the repo: no matches, so the
diff left no dangling reference to the removed player.

**Check 3 — assertions not weakened.** Rust: not weakened at all. Every Rust
change is a path or identifier rename; no assertion was edited, relaxed, or
removed, and `git diff main...HEAD | grep ignore` shows the diff adds no
`#[ignore]`. The one ignored daemon test
(`node_services_routes.rs:1078`, `two_gib_recording_upload_and_seek`) is
pre-existing. UI: genuinely weakened, 8 `it()` blocks → 1 — see finding 1 for
exactly what was lost and what still covers it.

**Check 4 — fixture `plugin.org`: CONFIRMED, and stronger than required.** It
is byte-identical to both the deleted example and the live marketplace copy:

    diff <(git show main:examples/plugins/meetings/plugin.org) \
         crates/orgasmic-core/tests/fixtures/plugins/plugin-min/plugin.org   → identical
    diff <marketplace plugins/meetings/plugin.org> \
         crates/orgasmic-core/tests/fixtures/plugins/plugin-min/plugin.org   → identical

So `:ID: meetings`, `:CHAT_PROMPT: prompts/meeting-chat.org` and
`:OPTIONAL: core.chat@1` are all preserved, and the fixture cannot silently
drift from the real plugin's manifest. That is fine and is the right call —
the tests key off the `meetings` collection and the chat prompt path.

The fixture does **not** need the Meetings player, confirmed three ways: the
manifest declares only `:UI: ui/index.js`, so `PluginManifest::read_dir` never
looks for `player.js`; the three daemon fixtures dropped `"ui/player.js"` from
their copy lists and still pass; and the fixture's own `ui/index.js` does not
import it.

The fixture is minimal — 4 files, each load-bearing:

| File | Lines | Why it must exist |
|---|---|---|
| `plugin.org` | 26 | manifest; asserted by `plugin.rs` test |
| `prompts/meeting-chat.org` | 32 | `:CHAT_PROMPT:`; `conversations_dispatch.rs:793` asserts the compiled prompt contains `recordings' moments` |
| `ui/index.js` | 14 | `:UI:` declared, must exist for `read_dir`; drives `PluginFixture.test.jsx` |
| `bin/import` | 30 | `:COMMANDS: import`; `plugin_cli.rs` actually executes it |

The prompt was trimmed 52 → 32 lines but deliberately keeps the
`recordings' moments` substring that `conversations_dispatch.rs:793` asserts,
so that assertion still bites. `bin/import` dropped the original's 1 MiB notes
ceiling and switched to `read_text()`; no test asserted that ceiling (the CLI
test body is otherwise unchanged and passes), and a size guard belongs in the
real plugin, not a fixture — so this is correct minimization, not a weakening.
Its `except` clause changed `ValueError` → `UnicodeError`, which is right:
`ValueError` was there to catch the removed 1 MiB `raise`, and `read_text` on
non-UTF-8 raises `UnicodeDecodeError`, a `UnicodeError`.

**Check 5 — section 12 matches the contract: CONFIRMED, 8 of 8.**
`PLUGINS-SCOPE.md:362-371` against the brief, item by item: clones under
`~/.orgasmic/user/marketplaces/<host>/<path>/` (`:364`) ✓; refresh is
`git pull` (`:365`) ✓; install copies in disabled (`:366`) ✓; enable is the
per-project trust step (`:366`) ✓; update replaces in place keeping activation,
re-approval on capability growth (`:367`) ✓; admin only (`:368`) ✓; CLI
`marketplace add|list|remove|refresh` (`:369`) ✓; `plugin add
<marketplace>/<id>` and `plugin update <id>` (`:370-371`) ✓. Decision 9 at
`:338` records the reversal with the date and quotes the old text, and
`## 11. Not in scope` correctly dropped "A plugin marketplace or index."
Findings 3 covers the one omission.

**Check 6 — all assigned suites pass.** Every exit code 0.

| Suite | Result |
|---|---|
| `cargo test -p orgasmic-core --lib plugin` | ok, **5 passed**, 0 failed (188 filtered) |
| `cargo test -p orgasmic-cli --test plugin_cli` | ok, **2 passed**, 0 failed |
| `cargo test -p orgasmic-daemon --test conversations_dispatch` | ok, **7 passed**, 0 failed |
| `cargo test -p orgasmic-daemon --test conversations_routes` | ok, **5 passed**, 0 failed |
| `cargo test -p orgasmic-daemon --test node_services_routes` | ok, **3 passed**, 0 failed, 1 ignored (pre-existing 2 GiB gate) |
| `cargo test -p orgasmic-daemon --test plugin_registry_routes` | ok, **2 passed**, 0 failed |
| `cd ui && npm ci && npm test` | ok, **78 files, 433 tests passed**, 0 failed |

No failures to classify. The single ignored test is pre-existing and
environment-gated by its own `#[ignore]` reason, not by this change.

**Extra probe, not in the brief — no data loss.** The diff deletes 484 lines of
plugin source on the premise that it "now lives at
github.com/theaspirational/orgasmic-plugins". I verified that premise rather
than trusting it: the repo is PUBLIC, `pushedAt` `2026-09-12T09:52:12Z` (about
three minutes before this task went `in_progress` at 09:55), and its tree
carries all six Meetings files including `ui/player.js`, plus a
`marketplace.org` index. The plugin code is genuinely preserved. Only the test
file was not carried over — finding 1.

**Residual risk.** Nothing in either repo exercises the Meetings plugin's JS,
so a change to the marketplace copy of `player.js` or `index.js` will not be
caught by any suite. The host modules those tests leaned on are independently
covered (`ui/src/lib/__tests__/nodeServices.test.ts`,
`useResourceResync.test.tsx`, and the three `pluginRuntime*.test.tsx` files),
and I confirmed the `@orgasmic/plugin-sdk` barrel
(`ui/src/plugin-sdk/index.ts`, aliased at `ui/vitest.config.ts:12`) was mocked
by the old test too — so this diff did not reduce barrel coverage; it was
already zero.

## Fix Directions

Ordered. None blocks this merge.

1. **File a follow-up for the Meetings plugin tests** (finding 1). Port
   `MeetingsPlayer.test.jsx` to `theaspirational/orgasmic-plugins`, or keep a
   trimmed local copy covering only the 30 s range clamp and the UTF-8-boundary
   4 KiB truncation. Those two are the client-side logic the host cannot
   validate.
2. **Bring the 2 GiB gate's command back into this repo** (finding 2). One doc
   comment above `crates/orgasmic-daemon/tests/node_services_routes.rs:1078`:

       // Run explicitly:
       // RUST_TEST_THREADS=1 cargo test -p orgasmic-daemon \
       //   --test node_services_routes two_gib_recording_upload_and_seek -- --ignored

   And drop the dead `MeetingsPlayer.test.jsx` line from the marketplace
   README's Verification block.
3. **Document `marketplace.org` in section 12** (finding 3): its schema, that
   `:SOURCE:` is what `plugin add <marketplace>/<id>` resolves, and whether
   "shallow clone" or `git pull` wins on a rewritten history.
4. **Reword** `crates/orgasmic-daemon/tests/conversations_routes.rs:186`
   (finding 4) to drop "the shipped example". Trivial; fold into any next
   touch of that file.
