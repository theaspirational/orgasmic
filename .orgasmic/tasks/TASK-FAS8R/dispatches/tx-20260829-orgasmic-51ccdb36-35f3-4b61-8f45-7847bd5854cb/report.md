## Changed

- Renamed `crates/orgasmic-cli/src/extract.rs` to `forum.rs` and replaced the top-level `extract` verb with a `forum` command group.
- Added the `forum ask` mode while preserving the existing ask pipeline flags and behavior; the old `extract` command has no alias.
- Renamed pipeline-level Rust identifiers, request-id prefixes, and the start diagnostic to `forum`/`ask` terminology while preserving the extractor, cross-reviewer, curator, extraction-stage, artifact, and report-only contracts.
- Updated the shipped orgasmic skill to route `/orgasmic forum`, renamed `references/extract.md` to `references/forum.md`, and documented mode selection (`ask` now, `critique` coming, more later).

## Verification Gates

- `cargo test -p orgasmic-cli --bin orgasmic forum::tests` — PASS, 6/6; includes `renderer_matches_stored_python_fixture`. Log: `/tmp/task-fas8r-forum-unit.log` (owner PID recorded in `/tmp/task-fas8r-forum-unit.pid`).
- `cargo test -p orgasmic-cli --test cli_parity` — PASS, 7/7; validates the shipped `orgasmic forum ask` invocation and flags against clap. Log: `/tmp/task-fas8r-cli-parity.log` (PID: `/tmp/task-fas8r-cli-parity.pid`).
- `cargo test -p orgasmic-cli --test dispatch manager_dispatch_status_close_done_with_stub_codex -- --exact` — PASS, 1/1; exercises report-only close acceptance and refusals. Log: `/tmp/task-fas8r-report-only.log` (PID: `/tmp/task-fas8r-report-only.pid`).
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings` — PASS. Log: `/tmp/task-fas8r-clippy.log` (PID: `/tmp/task-fas8r-clippy.pid`).
- `cargo fmt --check` — PASS. Log: `/tmp/task-fas8r-fmt.log`.
- `git diff --check` and `git diff HEAD --check` — PASS. Log: `/tmp/task-fas8r-diff-check.log`.
- `target/debug/orgasmic forum --help` — PASS; reports `Run multi-model deliberation modes` and lists `ask`. Output: `/tmp/task-fas8r-forum-help.txt`.
- `target/debug/orgasmic forum ask --help` — PASS; lists `--question`, `--question-file`, `--participant`, `--curator`, `--from`, `--artifact-id`, `--project`, and the retained `--timeout`. Output: `/tmp/task-fas8r-forum-ask-help.txt`.
- `git grep -n -i -F 'orgasmic extract' -- . ':!vendor/**'` — PASS with no matches. Proof log: `/tmp/task-fas8r-old-command-grep.log`.

## Unmet Criteria

- None.

## Residual Risk

- No live multi-model smoke was run, per the brief. The manager still needs to smoke `forum ask` from the merged binary during runtime reinstall.
