# Re-review TASK-1PFW6 round 2: marketplace registry fixes

Branch `task-1pfw6-impl-r2` has two commits over `main`: `5c7a8ad0` (the registry, reviewed once) and a fix commit on top. The first review (Opus, request changes) listed M1 to M6 and L1 to L7. The implementer (codex gpt-5.6-sol) reports every item fixed with a named test. Review the fix commit (`git diff HEAD~1`) against that list:

- M1 CLI 300 s timeout for marketplace add/refresh, plugin install/update.
- M2 daemon git in `spawn_blocking` with a bound; install/update stage before taking `plugins.operations` write guard. Read the guard ordering in `api.rs` and `marketplaces.rs` yourself; confirm no clone or copy happens under the write guard, and that the bound actually terminates a stalled child (which mechanism, and does it cover `git@`/ssh transports or only HTTP; say so).
- M3 browse continues past a bad index; refresh resets to pre-pull HEAD on invalid index; refresh-all continues per key.
- M4 git-URL `:SOURCE:` cached under `<marketplace>/.orgasmic-sources/<id>` at refresh; browse shows offered version and `update_available`. Check the cache dir cannot be addressed as a `:SOURCE:` folder by the index, and that the same path checks apply to the cached tree.
- M5 member token 403 for all seven mutations.
- M6 `path_segment` reuse; `?`/`#` rejected in keys.
- L1 to L7 as listed in the first review.

Also confirm the first-review "holds up" items still hold after the refactor (source containment, symlink refusal, `--` before git positionals, officials disable-only).

Run: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p orgasmic-core`, `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes`, `cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1` (the full CLI dispatch suite is known to flake under load; do not run it). Worktree-local `CARGO_TARGET_DIR`. No daemon.

Verdict: approve, approve-with-follow-ups, or reject. Anything left over that is not a bug goes to follow-ups, not a reject. Finalize with `orgasmic dispatch finalize`, verdict in the summary.
