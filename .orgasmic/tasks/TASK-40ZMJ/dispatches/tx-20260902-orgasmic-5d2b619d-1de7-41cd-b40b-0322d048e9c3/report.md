## Changed

- Added durable machine-local provider lockout memory at `$ORGASMIC_HOME/state/provider-lockouts.json`, populated only from a classified `provider_quota` terminal reason with a parseable `Retry-After` or one unambiguous passive `account.rate-limits.updated` deadline.
- Dispatch now refuses an active lockout as `provider_quota: <provider> locked until <RFC3339 time>` before acquire. `--force-preflight` deliberately overrides only this remembered quota refusal and records `FORCE_PREFLIGHT=true` on `manager.dispatch_started`.
- `manager drivers --health [--json]` overlays remembered active lockouts while providers without a quota signal remain `quota=unknown (no probe)`.
- Updated the shipped dispatch reference and added focused store/parser, watcher, CLI, and real daemon endpoint coverage.

## Verification Gates

- `cargo test -p orgasmic-core --lib exit_reason_tests --target-dir /tmp/orgasmic-task-40zmj-target` — 3 passed.
- `cargo test -p orgasmic-daemon --lib provider_quota::tests --target-dir /tmp/orgasmic-task-40zmj-target` — 2 passed.
- `cargo test -p orgasmic-daemon --lib quota_ --target-dir /tmp/orgasmic-task-40zmj-target` — 2 passed.
- `cargo test -p orgasmic-daemon --test dispatch_quota_lockout --target-dir /tmp/orgasmic-task-40zmj-target` — 1 passed; real HTTP dispatch refusal created no session, forced dispatch succeeded, tx carried the override.
- `cargo test -p orgasmic-daemon --test dispatch_preflight --target-dir /tmp/orgasmic-task-40zmj-target` — 1 passed; existing auth refusal remains intact.
- `cargo test -p orgasmic-cli --bin orgasmic health_listing_ --target-dir /tmp/orgasmic-task-40zmj-target` — 2 passed.
- `cargo test -p orgasmic-cli --bin orgasmic dispatch_request_carries_force_preflight --target-dir /tmp/orgasmic-task-40zmj-target` — 1 passed.
- `cargo test -p orgasmic-cli --test cli_parity --target-dir /tmp/orgasmic-task-40zmj-target` — 9 passed.
- Synthesized production CLI probe: `manager drivers --health` printed `codex auth=ok quota=locked until 2099-01-01T00:00:00Z`; every provider without memory remained `quota=unknown (no probe)` (`/tmp/TASK-40ZMJ-health.log`).
- `cargo clippy --workspace --all-targets --target-dir /tmp/orgasmic-task-40zmj-target -- -D warnings` — clean (`/tmp/TASK-40ZMJ-final-clippy.log`).
- `cargo fmt --all -- --check` and `git diff --check` — clean.
- During implementation, one compile regression (missing new test initializer) and one invalid test fixture filename were found and corrected; all final reruns above are green.

## Unmet Criteria

- None.

## Residual Risk

- A quota termination that supplies neither a parseable retry value nor one unambiguous passive reset deadline is deliberately not cached; health remains `unknown (no probe)` rather than inventing an expiry.
- The laptop-prohibited full workspace test suite was not run; targeted production-path tests cover the changed paths.
