# TASK-EHE15 — `orgasmic artifact comments` read verb

## Changed
- `crates/orgasmic-cli/src/artifact.rs` (only file touched; `main.rs` needed no
  edit — the new `ArtifactCmd::Comments` variant flows through the existing
  subcommand wiring):
  - New verb `orgasmic artifact comments <ART-ID> [--project <id>]
    [--include-consumed]`.
  - Reuses `GET /api/artifacts/:id?project=..&include_consumed=..` via the
    existing `DaemonClient` pattern (same client/project resolution as
    `submit`/`feedback`). No daemon edits.
  - Prints a pretty-JSON array (house style: `serde_json::to_string_pretty`,
    same as `node`/`drivers` read verbs), one entry per comment: `cid`,
    `author`, `time`, `message`, `anchor`, `consumed`. The stored anchor JSON
    string is parsed to an object (QuestionForm `questionKey`/`answer` come
    through structured), falling back to the raw string if unparseable.
  - 404 from the daemon is mapped to `artifact <ID> not found`, naming the id.
  - Consumed filtering stays daemon-side: the CLI forwards
    `include_consumed=false` by default, `true` with the flag.

## Verification Gates
- `cargo test -p orgasmic-cli --bin orgasmic artifact::tests` (focused, per
  brief — never the whole crate): **3 passed, 0 failed** —
  - `comments_prints_cid_author_time_message_anchor_consumed` — mock daemon
    (`RecordingDaemon` from `test_support`), asserts the requested path is
    `/api/artifacts/ART-TESTA?project=proj-1&include_consumed=false` and that
    each rendered entry carries cid/author/time/message/anchor(parsed
    questionKey+answer)/consumed.
  - `include_consumed_flag_is_forwarded_to_the_daemon` — asserts
    `include_consumed=true` in the request path. The hide/show behavior itself
    is enforced and already tested daemon-side (`load_artifact_detail`
    `retain(|c| !c.consumed)` + the `get_artifact` tests around
    `crates/orgasmic-daemon/src/api.rs:35947`), which the brief forbids
    re-touching.
  - `unknown_artifact_error_names_the_id` — 404 body
    `{"error":"artifact not found"}` surfaces as an error containing
    `ART-GONE1`.
- Clap wiring smoke: built the real binary; `orgasmic artifact comments --help`
  prints the expected usage/flags.

## Unmet Criteria
- Criterion 1's **time** field is emitted as `null`, not a real timestamp. The
  brief's premise ("comment shape … CID, author, time …") is wrong on this one
  field: the endpoint's `CommentRecord`
  (`crates/orgasmic-daemon/src/artifacts.rs:101`) has no time field. The
  journal `:TIME:` exists (`JournalEntry.time`,
  `crates/orgasmic-core/src/node_kernel.rs:88`) but `parse_comments`
  (`crates/orgasmic-daemon/src/artifacts.rs:587`) drops it, so
  `GET /api/artifacts/:id` never serves it. Fixing needs a ~2-line daemon
  edit (add `time` to `CommentRecord`, map `entry.time`), which the brief
  ("CLI-only change; no daemon edits") and my write scope both forbid. The CLI
  keeps a schema-stable `"time": null` with a `ponytail:` marker so it
  populates trivially once the daemon serves it. Everything else in all three
  criteria is met.

## Residual Risk
- 404 detection matches the daemon's `"artifact not found"` error body string;
  if that message is ever reworded, the CLI falls back to the raw
  `daemon returned 404 …` error (still fails clearly, just without naming the
  id).
- No live-daemon end-to-end run: the tests use the crate's standard
  `RecordingDaemon` mock. The daemon side of the contract (detail shape,
  consumed filtering, real HTTP) is covered by the daemon's own tests against
  the same endpoint.
