## Changed

- Removed the in-repo Meetings example and redirected every former consumer to `crates/orgasmic-core/tests/fixtures/plugins/plugin-min/`.
- Added the minimal fixture surface exercised by tests: one node type, one command, one UI registration module, and one chat prompt. Replaced the Meetings player UI test with a focused fixture SDK registration/write test.
- Reversed PLUGINS-SCOPE decision 9, removed marketplaces from section 11, and added the requested section 12 marketplace storage, trust, update, administration, and CLI contract.
- Updated the plugin-author skill to install Meetings from the official marketplace.
- No production code changed.

## Verification Gates

- `cargo fmt --all` — passed.
- `CARGO_TARGET_DIR=$PWD/target/task-n4qmc cargo test -p orgasmic-core --lib plugin` — passed, 5 tests. Log: `/tmp/TASK-N4QMC-core.log`.
- `CARGO_TARGET_DIR=$PWD/target/task-n4qmc cargo test -p orgasmic-cli --test plugin_cli` — passed, 2 tests (re-run after the final test rename). Log: `/tmp/TASK-N4QMC-cli-final.log`.
- `CARGO_TARGET_DIR=$PWD/target/task-n4qmc cargo test -p orgasmic-daemon --test conversations_dispatch --test conversations_routes --test node_services_routes --test plugin_registry_routes` — passed, 17 tests; the explicit 2 GiB streaming gate remained ignored. Log: `/tmp/TASK-N4QMC-daemon.log`.
- `CARGO_TARGET_DIR=$PWD/target/task-n4qmc cargo test -p orgasmic-daemon --lib --test boot_resilience --test recovery_fault_restart --test dispatch_endpoint required_test_tooling_is_present` — passed, 4 tooling sentinels. Log: `/tmp/TASK-N4QMC-tooling.log`.
- `cd ui && npm ci` — passed. Log: `/tmp/TASK-N4QMC-npm-ci.log`.
- `cd ui && npm test` — passed, 78 files and 433 tests. Log: `/tmp/TASK-N4QMC-ui.log`.
- `rg examples/plugins` — no matches (exit 1).
- `git diff --cached --check` — passed.

## Unmet Criteria

None.

## Residual Risk

- The intentionally ignored 2 GiB recording smoke was not run explicitly.
- `npm ci` reported 22 dependency audit findings (3 low, 8 moderate, 10 high, 1 critical); dependency remediation is outside this task.
- The marketplace CLI verbs are documented here but intentionally implemented by the two later tasks named in the brief.
