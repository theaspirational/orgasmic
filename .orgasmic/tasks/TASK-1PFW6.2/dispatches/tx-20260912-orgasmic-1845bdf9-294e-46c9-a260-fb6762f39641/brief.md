# Review TASK-1PFW6.2: marketplace source cache round 3

One commit over `main` (`git diff main...HEAD`), Rust only, four files. Implementer was codex gpt-5.6-sol. It closes the eight follow-ups from the previous Opus review of TASK-1PFW6.1:

1. `plugins()` never spawns git; uncached git sources report `error: "source not cached; run marketplace refresh"` with null version; `GET /plugins` does one browse per request; `plugins()` documents that it takes `operations`.
2. Browse reads a valid cached manifest even when the last refresh recorded a transport error (version and capabilities alongside the error).
3. Refresh of a git source clones fresh into a sibling temp dir and swaps after validation; a marketplace `git pull` failure (including a stale `index.lock`) falls back to a fresh clone and swap.
4. Git children run in their own process group; timeout kills and reaps the group. The unit test checks both shell and child pids are gone.
5. Cache path is `marketplace-sources/<key>/.sources/<id>`.
6. Refresh response carries `sources: [{id, ok, error}]`.
7. Refresh prunes source ids absent from the index; first marketplace access deletes legacy `<clone>/.orgasmic-sources/`.
8. `file://localhost/x` and `file:///x` key the same.

Verify each against the diff. Then look hard at: the swap (temp dir sibling under the same filesystem so rename is atomic; old cache removed after, not before; a failed validation leaves the old cache intact); the fresh-clone fallback for the marketplace clone itself (does it preserve the user record and `official` flag; does it run under `operations`); `GET /plugins` truly has zero git spawns on any path including `offering()`; `killpg` cannot hit pgid 0 or the daemon's own group if spawn failed; the legacy cleanup cannot delete anything outside `<clone>/.orgasmic-sources`.

Run: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p orgasmic-core`, `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes`, `cargo test -p orgasmic-daemon --lib marketplaces`, `cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1`. Worktree-local `CARGO_TARGET_DIR`. No daemon.

Verdict: approve, approve-with-follow-ups, or reject, file and line per finding. Finalize with `orgasmic dispatch finalize`, verdict in the summary.
