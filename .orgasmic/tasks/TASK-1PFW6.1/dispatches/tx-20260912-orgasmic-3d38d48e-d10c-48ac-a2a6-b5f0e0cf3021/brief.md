# Review TASK-1PFW6.1: marketplace registry follow-ups

One commit over `main` (`git diff main...HEAD`), Rust only. Implementer was codex gpt-5.6-sol. It closes the seven follow-ups from the round 2 review of TASK-1PFW6:

1. Git child bounded: `tokio::process::Command`, `kill_on_drop(true)`, 120 s `tokio::time::timeout`, `GIT_TERMINAL_PROMPT=0`, `GIT_SSH_COMMAND` batch mode with connect timeout, HTTP low-speed envs kept. Verify a timed-out child is actually killed and reaped, the `operations` mutex is released on timeout, and the next `marketplace add` succeeds. Confirm the wrapper's timeout unit test really spawns a sleeping child.
2. Browse: per-entry `error` with null version, no abort of a marketplace on one bad entry, no `record_error` from the read path. Confirm `marketplace list` no longer shows a sticky error from a browse race.
3. Source cache moved to `~/.orgasmic/user/marketplace-sources/<key>/<id>/`. Confirm nothing is written under the clone anymore and the rename-swap is still atomic.
4. `validate_relative_source` rejects a leading component starting with `.orgasmic`.
5. Lazy source clone on browse/install; refresh only re-pulls cached sources, continues past failures, rolls back an invalid source. Check: does a lazy clone on browse happen under a lock so two concurrent browses do not clone twice into the same path? Is a lazy clone on a GET acceptable (a read request with a 120 s side effect)? State your judgement; if you would rather have browse never clone and only install clone, say so as a follow-up, not a reject, unless it is a correctness bug.
6. Duplicate `path_segment` deleted from `manager.rs`.
7. `file://` with a host other than empty or `localhost` rejected in `marketplace_key`.

Also recheck the trust boundary after this refactor: source containment, symlink refusal, `--` before every git positional, officials disable-only, admin-only mutations.

Run: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p orgasmic-core`, `cargo test -p orgasmic-daemon --test plugin_registry_routes --test marketplace_routes`, `cargo test -p orgasmic-daemon --lib marketplaces`, `cargo test -p orgasmic-cli --test plugin_cli --test cli_parity -- --test-threads=1`. Worktree-local `CARGO_TARGET_DIR`. No daemon.

Verdict: approve, approve-with-follow-ups, or reject, with file and line per finding. Finalize with `orgasmic dispatch finalize`, verdict in the summary.
