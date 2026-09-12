# Task 1 of 3: move Meetings out, add a test fixture plugin, reverse decision 9

## Context

Orgasmic is getting plugin marketplaces (git repos with a `marketplace.org` index). The Meetings example plugin has moved to its own repos already:
- https://github.com/theaspirational/orgasmic-plugins (public official marketplace)
- https://git.ath/orgasmic/plugins (internal edge, same content)
Both hold `marketplace.org` and `plugins/meetings/`. Nothing in this repo may reference `examples/plugins/meetings` after this task.

Two later tasks add the daemon/CLI marketplace registry and the UI page. This task only cleans the ground.

## Do

1. Delete `examples/plugins/meetings` from this repo (`git rm -r`).
2. Add a minimal fixture plugin at `crates/orgasmic-core/tests/fixtures/plugins/plugin-min/` (or the smallest location every crate can reach with `env!("CARGO_MANIFEST_DIR")`). It must satisfy every test that used the Meetings example. Today those are:
   - `crates/orgasmic-cli/tests/plugin_cli.rs`
   - `crates/orgasmic-core/src/plugin.rs` (the test near line 417)
   - `crates/orgasmic-daemon/tests/conversations_dispatch.rs`
   - `crates/orgasmic-daemon/tests/conversations_routes.rs`
   - `crates/orgasmic-daemon/tests/node_services_routes.rs`
   - `ui/src/components/__tests__/MeetingsPlayer.test.jsx`
   Run `rg -n "examples/plugins" --glob '!node_modules' --glob '!target'` to find any others. The fixture keeps only what those tests exercise: one node type, one command, one UI module, one chat prompt if a test needs it. Read each test before deciding what the fixture needs. Do not port the Meetings player UI; if `MeetingsPlayer.test.jsx` cannot be satisfied by a small fixture, move that test's subject into the fixture's `ui/` as the smallest module that still proves the SDK behaviour the test asserts, and rename the test.
3. Edit `PLUGINS-SCOPE.md`:
   - Decision 9 row becomes: `Sharing | Git marketplaces. A marketplace is a git repo with a marketplace.org index. Entries point at a folder in that repo or a git URL. Shipped official list holds github.com/theaspirational/orgasmic-plugins only. Reversed 2026-09-12 from "Git URL or tarball only. No index."`
   - Remove `A plugin marketplace or index.` from section 11.
   - Add a short section `12. Marketplaces` (max 25 lines): storage `~/.orgasmic/user/marketplaces/<host>/<path>/` as shallow clones, refresh is `git pull`, install copies the plugin folder in disabled, enable stays the per-project trust decision, update replaces files in place and keeps enable state, capability growth forces re-approval, admin only for add/remove/refresh/install/update, CLI verbs `orgasmic marketplace add|list|remove|refresh` and `orgasmic plugin add <marketplace>/<id>` and `orgasmic plugin update <id>`.
4. Update `shipped/skills/orgasmic-plugin-author/SKILL.md` and any doc that tells the reader to install Meetings from `examples/plugins/meetings`: point to the marketplace instead (`orgasmic marketplace add https://github.com/theaspirational/orgasmic-plugins` then `orgasmic plugin add orgasmic-plugins/meetings`). Keep edits minimal.

## Verify

```sh
cargo fmt --all
cargo test -p orgasmic-core --lib plugin
cargo test -p orgasmic-cli --test plugin_cli
cargo test -p orgasmic-daemon --test conversations_dispatch --test conversations_routes --test node_services_routes --test plugin_registry_routes
cd ui && npm ci && npm test
rg -n "examples/plugins" --glob '!node_modules' --glob '!target'   # must print nothing
```

Use a worktree-local `CARGO_TARGET_DIR` (never one shared with another checkout). Do not start a daemon; tests are daemon-free. Report which tests ran and their result.

## Scope

Write: `examples/`, `crates/**/tests/**`, the single test in `crates/orgasmic-core/src/plugin.rs`, `ui/src/components/__tests__/**`, `PLUGINS-SCOPE.md`, `shipped/skills/**`. No production code changes. Commit once with a clear message, then `orgasmic dispatch finalize`.
