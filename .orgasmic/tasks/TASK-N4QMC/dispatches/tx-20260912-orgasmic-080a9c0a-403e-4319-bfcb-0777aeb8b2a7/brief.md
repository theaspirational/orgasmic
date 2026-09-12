# Review TASK-N4QMC: Meetings moved out, fixture plugin, decision 9 reversed

Review the single commit on this branch against `main` (`git diff main...HEAD`). The implementer was codex gpt-5.6-sol. Its brief:

- Delete `examples/plugins/meetings` (it now lives at github.com/theaspirational/orgasmic-plugins).
- Add a minimal fixture plugin `crates/orgasmic-core/tests/fixtures/plugins/plugin-min` that satisfies every test that used the example (five Rust tests, one UI test).
- Edit `PLUGINS-SCOPE.md`: decision 9 reversed to git marketplaces, marketplace removed from section 11, new section 12 with the marketplace contract.
- Point `shipped/skills/orgasmic-plugin-author/SKILL.md` at the marketplace.
- No production code changes.

Check:
1. No production code changed (only tests, fixtures, docs).
2. `rg -n "examples/plugins" --glob '!node_modules' --glob '!target'` is empty.
3. The fixture is minimal and each test still asserts what it asserted before, not a weakened version. Compare the deleted `MeetingsPlayer.test.jsx` with the new `PluginFixture.test.jsx`: was real coverage lost that a later task must restore? Name it if so.
4. The fixture's `plugin.org` still declares id `meetings`, a `:CHAT_PROMPT:` and `:OPTIONAL: core.chat@1` because tests depend on them. Confirm that is fine and the fixture does not need the Meetings player.
5. Section 12 of the scope doc matches this contract: clones under `~/.orgasmic/user/marketplaces/<host>/<path>/`, refresh is `git pull`, install copies in disabled, enable is the per-project trust step, update replaces in place keeping activation and re-approval on capability growth, admin only, CLI verbs `marketplace add|list|remove|refresh`, `plugin add <marketplace>/<id>`, `plugin update <id>`.
6. Run: `cargo test -p orgasmic-core --lib plugin`, `cargo test -p orgasmic-cli --test plugin_cli`, `cargo test -p orgasmic-daemon --test conversations_dispatch --test conversations_routes --test node_services_routes --test plugin_registry_routes`, `cd ui && npm ci && npm test`. Use a worktree-local `CARGO_TARGET_DIR`. Do not start a daemon.

Verdict: approve or request changes with exact file and line. Finalize with `orgasmic dispatch finalize` and put the verdict in the summary.
