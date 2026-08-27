# Brief: TASK-EHE15 — `orgasmic artifact comments` read verb

Add a CLI verb that lists an artifact's comments so a griller round can read
clicked QuestionForm answers without hand-rolled HTTP.

Anchors:
- `crates/orgasmic-cli/src/artifact.rs` — existing `blocks`/`submit`/`feedback`
  verbs; follow their daemon-client pattern and error style.
- Daemon already serves it: `GET /api/artifacts/:id` (`get_artifact` in
  `crates/orgasmic-daemon/src/api.rs`) returns `ArtifactDetail` including
  comments; `?include_consumed=true` includes consumed ones. Reuse this
  endpoint — do not add a new daemon route.
- Comment shape: see `crates/orgasmic-daemon/src/artifacts.rs` (CID, author,
  time, message, anchor JSON, consumed/resolution state).

Shape:
`orgasmic artifact comments <ART-ID> [--project <id>] [--include-consumed]`
prints JSON: one entry per comment with cid, author, time, message, anchor,
consumed. Default hides consumed comments. Unknown id → clear error naming it.

Constraints:
- CLI-only change; no daemon edits.
- Run only the focused test (`cargo test -p orgasmic-cli --bin orgasmic artifact`
  or the matching integration test file) — NEVER the whole crate or workspace.

Acceptance: the three criteria on the task node.
Report per output contract; name files touched and show test output.
