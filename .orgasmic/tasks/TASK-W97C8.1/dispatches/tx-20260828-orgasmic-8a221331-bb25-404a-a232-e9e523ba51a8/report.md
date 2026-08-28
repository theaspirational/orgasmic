# Changed
- `crates/orgasmic-daemon/src/api.rs`: dispatch start now writes the compiled bundle to `.orgasmic/tmp/dispatch/<stem>/<stem>-compiled-prompt.md`; it no longer creates or writes the durable dispatch record directory.
- `crates/orgasmic-core/src/paths.rs` and `src/lib.rs`: close validates no-follow handles for the tmp brief and compiled prompt, promotes them as `brief.md` and `compiled-prompt.md` with the report/evidence/stdout record, and unlinks all tmp sources only after every copy succeeds.
- `crates/orgasmic-cli/src/manager.rs`: close supplies the recorded brief path to the validated promotion path; rollback retains its tmp-only cleanup path.
- `crates/orgasmic-cli/tests/dispatch.rs`, `tests/shipped_conventions.rs`, and `shipped/prompt-studio/conventions/manager-dispatch.org`: pin absent-before-close, complete close-time promotion/commit, rollback without a durable orphan, and document the new timing.

The tmp copies live at `.orgasmic/tmp/dispatch/<stem>/<stem>-brief.md` and `.orgasmic/tmp/dispatch/<stem>/<stem>-compiled-prompt.md`, beside the attempt-specific `last.txt` and `stdout.log` files.

# Verification Gates
- `rustup run 1.97.1 cargo test -p orgasmic-core --lib paths::` — 14 passed.
- `rustup run 1.97.1 cargo test -p orgasmic-cli --bin orgasmic dispatch_close` — 11 passed.
- `rustup run 1.97.1 cargo test -p orgasmic-cli --bin orgasmic dispatch_evidence` — 5 passed.
- `ORGASMIC_ALLOW_MISSING_TOOLS=tmux rustup run 1.97.1 cargo test -p orgasmic-cli --test dispatch dispatch_close_promotes_complete_record_only_at_close -- --nocapture` — 1 passed.
- `ORGASMIC_ALLOW_MISSING_TOOLS=tmux rustup run 1.97.1 cargo test -p orgasmic-cli --test dispatch dispatch_timeout_requests_daemon_cleanup -- --nocapture` — 1 passed.
- `rustup run 1.97.1 cargo test -p orgasmic-cli --test shipped_conventions` — 5 passed.
- `git diff --check` — passed.
- One initial `cargo test -p orgasmic-cli --lib ...` probe exited 101 because `orgasmic-cli` is bin-only; corrected to the `--bin orgasmic` gate above.

# Unmet Criteria
- None.

# Residual Risk
- Focused gates only, as requested; no full workspace suite or clippy run.
