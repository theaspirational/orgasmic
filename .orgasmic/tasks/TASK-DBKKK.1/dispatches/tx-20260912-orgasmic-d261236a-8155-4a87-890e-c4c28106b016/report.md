# Changed

1. Broken installs: `PluginsView` now keeps `manifest || error` rows, renders manifest-less failures as degraded rows with the raw id, destructive error text, and only Remove, and combines `legacy-descriptors` / `activation` failures into one page-level alert. Covered by `renders a degraded plugin row with Remove` and `combines daemon pseudo-id errors into one page alert`.
2. Remove confirmation: plugin and non-official marketplace Remove buttons now route through the existing `AlertDialog`; plugin copy states the machine-wide scope and retained node data, while marketplace copy states clone/record deletion. Covered by `confirms plugin removal before posting` and `confirms marketplace removal before posting`.
3. Admin gate: mutation controls now require `useMe().identity === 'admin'`; the test mock and member assertion use identity directly. Covered by `keeps mutation controls disabled for a non-admin`.
4. Recommendation refresh: marketplace add and marketplace enable/disable now refresh plugins as well as marketplace/catalog resources; null-marketplace recommendations show `No marketplace offers this plugin.` with no Install action. Covered by `adds a marketplace by git URL`, `refreshes plugins after enabling a marketplace`, and `explains when no marketplace offers a recommended plugin`.
5. Marketplace copy: recommendation text resolves the offering marketplace's `name ?? alias ?? key`; the existing `renders the three tabs and recommended plugin card` assertion covers the configured marketplace name.
6. Requested UI paths: added coverage for path-encoded marketplace refresh and plugin update routes, grown-capability re-approval, Browse install, and Browse search. Covered by `refreshes a path-encoded marketplace`, `updates a plugin through its path-encoded id`, `re-approves the full grown capability set`, `installs a plugin from Browse`, and `filters Browse by plugin id or description`.

Files changed:
- `ui/src/components/PluginsView.tsx`
- `ui/src/components/__tests__/PluginsView.test.tsx`

# Verification Gates

- `cd ui && npm ci` — passed; 905 packages installed. Log: `/tmp/TASK-DBKKK.1-npm-ci.log`.
- `cd ui && npm run typecheck` — passed.
- `cd ui && npm test` — passed: 79 files, 449 tests. Final log: `/tmp/TASK-DBKKK.1-ui-test-final.log`.
- `cd ui && npm run build` — passed, including its typecheck and Vite production build. Final log: `/tmp/TASK-DBKKK.1-ui-build-final.log`.
- `git diff --check` — passed.
- No Rust files, daemon code, or ledger files in the source worktree were changed.

# Unmet Criteria

None.

# Residual Risk

- `npm ci` reports 22 dependency audit findings (3 low, 8 moderate, 10 high, 1 critical); dependency remediation is outside this UI task.
- The passing test/build output retains existing CSS optimizer and large-chunk warnings; this change adds no dependency or bundle architecture work.
