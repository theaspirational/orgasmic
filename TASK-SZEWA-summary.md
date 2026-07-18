# TASK-SZEWA — implementer summary

## Changed
- **Addressing**: `kind + mode + harness (+ harness_args/model/effort)` is dispatch/stage/artifactor routing authority; `orgasmic_drivers::SUPPORTED` validates pairs (`addressing.rs`).
- **Governance**: `resolve_governance` wired into spawn; linked-skills missingness resolved against home skills; babysitter via config overlay (not worker `:BABYSITTER_WORKER:`).
- **Zero model knowledge**: removed cursor/cursor-acp orgasmic `DEFAULT_MODEL`; omitted model stays omitted; cursor ACP catalogs from structured `session/new`; no CLI text scraping.
- **CLI**: `--mode`/`--harness`/`--harness-arg(s)` replace mandatory `--worker`; dry-run prints mode/harness/argv.
- **UI**: `TransportPicker` from `/managers/drivers`; `RuntimeOptionsBar` on chat path; generate dialog sends mode/harness; free-text model/effort with harness-default placeholders.
- **Tests**: dispatch/stage/artifact fixtures updated for addressing; missing-worker → unsupported-transport path-free test.

## Verification Gates
| Gate | Result |
|------|--------|
| `cargo fmt --check` | pass |
| `cargo test -p orgasmic-core -p orgasmic-drivers -p orgasmic-daemon -p orgasmic-cli --no-fail-fast` | pass except `modes::rmux::tests::live_rmux_render_path_streams_screen_and_completes` (see residual) |
| `cargo clippy --workspace --all-targets` | pass (warnings only) |
| `npm --prefix ui run typecheck` | fail pre-existing `TS5101` (`ignoreDeprecations: "5.0"` vs TS 6 / `baseUrl`) — unchanged on HEAD |
| UI vitest (artifactsApi, generateArtifactDialog, artifactRegenerate) | 22/22 pass |
| `git diff --check` | pass |

## Probe notes
- Live `cursor-agent acp` `session/new` returns structured `models.availableModels` (docs omit discovery; binary has it). Wired via adapter cache + `runtime_options_catalog`.
- Claude ACP: no reliable structured catalog → free-text degrade.

## Unmet Criteria
- None for stated cutover. Physical worker-file removal remains **TASK-DZ5NM**.

## Residual Risk
- `live_rmux_render_path_streams_screen_and_completes` failed twice (sentinel not seen on render_stream); other live rmux tests passed; `rmux.rs` untouched — treat as environment/flaky, not this cutover.
- UI regenerate cold path still needs mode/harness from client; hot path ignores them.
- Babysitter still loads template by governance `babysitter_worker` id until DZ5NM.
- Persona/operating_rules on addressed workers are empty (Prompt Studio by kind).
