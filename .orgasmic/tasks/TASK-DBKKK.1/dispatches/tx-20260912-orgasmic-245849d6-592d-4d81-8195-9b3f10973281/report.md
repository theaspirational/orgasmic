# Review TASK-DBKKK.1 — Plugins page follow-ups

Commit under review: `9aa74bd1` ("Show broken plugin installs, confirm removes, and
gate the Plugins page by identity"), one commit over `main`, two files
(`ui/src/components/PluginsView.tsx`, `ui/src/components/__tests__/PluginsView.test.tsx`).

## Verdict

**approve-with-follow-ups.** All six follow-ups are implemented and the UI gate is
green (typecheck, 449/449 tests, production build). No HIGH findings: nothing here
loses data, escalates privilege, or fails an acceptance criterion. The findings below
are stale-state and dead-end-affordance defects on paths the same follow-up set opened,
plus test gaps.

## Findings

### P2 MEDIUM (bug, stale state) — `ui/src/components/PluginsView.tsx:325`, `:374`: marketplace Refresh and Remove still do not refresh `plugins`, so a recommendation card keeps naming a marketplace that no longer offers the plugin

Follow-up 4 was applied to marketplace **add** (`:105`) and **enable/disable** (`:319`)
but not to **refresh** (`:325`) or **remove** (`:374`). `GET /plugins` derives the
recommendation's `marketplace` field from `MarketplaceRegistry::offering()`
(`crates/orgasmic-daemon/src/api.rs:16055` → `crates/orgasmic-daemon/src/marketplaces.rs:339`),
which iterates only *enabled and cloned* marketplaces. Both unfixed paths change that
set:

- `marketplaces.rs:154-158` — `refresh` clones a marketplace whose root is missing.
  Reproduction: add a marketplace while the clone fails (row shows `Unavailable`), then
  click **Refresh**. Browse repopulates (`catalog.refresh` runs) but the Installed tab's
  recommendation card still reads "No marketplace offers this plugin." and renders no
  Install button, because `plugins` was never refetched.
- `marketplaces.rs:176-193` — `remove` deletes the record and clone. Reproduction: remove
  the only marketplace offering a recommended plugin. The recommendation card still reads
  "Install X from <deleted marketplace>" with an **enabled** Install button; clicking it
  posts `/plugins/install` and gets a 400 (`resolve_key` → "unknown marketplace") in the
  error panel.

Both stay wrong until an unrelated resync or a `visibilitychange` fires
(`ui/src/lib/useResource.ts:70-82`). Fix: add `plugins.refresh` to the refresh array on
both lines, exactly as `:105`/`:319` do.

### P2 MEDIUM (correctness/design) — `ui/src/components/PluginsView.tsx:110-112`: the same broken plugin renders twice — as a degraded row *and* as a "Recommended → Install" card

`installed` (`:110`) now admits `plugin.error` rows and `recommended` (`:112`) filters
only on `plugin.recommended`. The daemon sets both flags on one entry: a plugin whose
manifest breaks *after* it was enabled has `manifest: null` + `error: Some(...)`
(`crates/orgasmic-daemon/src/plugins.rs:251`) and, because its id is still in
`plugin-ownership.json` and its collection directory is non-empty,
`recommended: true` with `marketplace: Some(key)`
(`crates/orgasmic-daemon/src/api.rs:16048-16056`). That is precisely the "null manifest
plus error" state follow-up 1 targets.

Result: the user sees "This project has X nodes. Install X from Y." with a live Install
button above a degraded row saying the manifest failed to parse. Install cannot work —
`prepare_install` fails `ensure!(!destination.exists(), "plugin is already installed")`
(`marketplaces.rs:266`) — so the card is a dead end that contradicts the row directly
below it. Fix direction: exclude ids that are already present with an error from
`recommended` in the UI (`!plugin.error`), or stop the daemon marking an id with a
present-but-broken install as recommended.

### P3 LOW (correctness) — `ui/src/components/PluginsView.tsx:43`: `PLUGIN_ERROR_IDS` unconditionally hides any plugin genuinely named `activation` or `legacy-descriptors`

`installed` drops an entry when `PLUGIN_ERROR_IDS.has(plugin.id)` regardless of whether
it has an error (`:110`). `orgasmic_core::plugin::validate_id`
(`crates/orgasmic-core/src/plugin.rs:33-45`) accepts both strings — lowercase letters and
hyphens only — and neither is reserved by core, so a marketplace may legitimately ship
either id. Such a plugin would never appear in the Installed list and would be
unremovable and un-toggleable from the UI. No marketplace ships one today, so this is
latent. Fix direction: key the pseudo-id alert on `manifest === null && error !== null &&
!installedIds.has(id)`, or have the daemon namespace its pseudo-ids (e.g. `!activation`)
so they cannot collide with a valid plugin id.

### P3 LOW (usability) — `ui/src/components/PluginsView.tsx:355`: the marketplace-remove confirm states the clone deletion but not the update consequence

"Deletes the local clone and record for X." is accurate as far as it goes — `remove`
touches only the record and the clone (`marketplaces.rs:184-191`), installed plugins in
`~/.orgasmic/plugins` survive. But plugins installed from it silently lose updates:
`prepare_update` re-resolves the source marketplace (`marketplaces.rs:303-305`), so
after removal the Update button simply stops appearing (the catalog no longer lists the
plugin) with no explanation anywhere. The follow-up asked for the scope to be stated;
one clause — "Plugins installed from it stay, but stop receiving updates." — closes it.

### P3 LOW (usability) — `ui/src/components/PluginsView.tsx:145`: the recommendation card briefly shows a raw marketplace key

`marketplaceTitle` falls back to `key` when `marketplaces.data` is still `null` (`:49-52`).
The three resources load independently, so if `/plugins` resolves before `/marketplaces`
the card renders "Install Meetings from github.com/theaspirational/orgasmic-plugins."
and then swaps to the display name. Cosmetic flicker only; gate it on
`marketplaces.data` if it bothers anyone.

### P3 LOW (test) — gaps in the new suite

The seven added tests are real (each asserts a posted route or rendered copy, none are
tautological), but these paths are untested:

1. The dual-render in the MEDIUM finding above — the degraded-row test uses
   `recommended: false` (`PluginsView.test.tsx:189-192`), so it cannot catch it.
2. The missing `plugins.refresh` on marketplace refresh/remove — the two refresh-count
   assertions cover only add (`:143`) and enable (`:166`).
3. **Cancel**: no test asserts `pending` clears and nothing posts after Cancel.
4. **Failure**: no test asserts a rejected mutation renders `ErrorPanel` and leaves the
   list usable — the whole `mutationError` path is unexercised in this file.

## Verification Notes

Everything below is from this worktree at `9aa74bd1`; no daemon was run.

**Gate (assigned by the brief), all green.** `cd ui && npm ci && npm run typecheck &&
npm test -- --run && npm run build`, log `/tmp/dbkkk1-gate.log`, owner PID 96864:
- `npm ci` — 905 packages.
- `tsc --noEmit` — clean. This also confirms `AlertDialogAction` legitimately accepts
  `variant`/`size` (`ui/src/components/ui/alert-dialog.tsx:150-165` picks them from
  `Button`) and that `MarketplaceStatus` is exported from `@/lib/api`.
- Vitest — **79 files / 449 tests passed**, 0 failed.
- `vite build` — `built in 8.98s`.

**Follow-up 1 (degraded rows) — verified end to end.** The row is reachable only for a
real broken install: the only source of `manifest: null` + `error` for a non-pseudo id is
the per-directory manifest failure at `plugins.rs:251`; the other two error inserts
("ownership cannot change", "capabilities grew", `plugins.rs:321-331`) always carry a
manifest. Its Remove therefore succeeds — `PluginRegistry::remove` (`plugins.rs:462-485`)
requires only a non-symlink directory, never a parseable manifest. Both pseudo-ids are
real daemon output, confirmed at `plugins.rs:226` (`legacy-descriptors`) and
`plugins.rs:307` (`activation`).

**Follow-up 3 (admin gate) — matches the backend exactly.** `identity === 'admin'`
(`PluginsView.tsx:65`) is true iff there is no member session
(`ui/src/hooks/useMe.tsx`, `identity: isMember ? 'member' : 'admin'`). Every plugin and
marketplace mutation route is admin-only by omission from `MEMBER_ALLOWED_ROUTES`
(`crates/orgasmic-daemon/src/api.rs:965-1008`, enforced at `:1087-1106`) — members are
granted only `GET /marketplaces`, `GET /marketplaces/plugins`, `GET /plugins`,
`POST /plugins/:id/run` and `POST /plugins/run/revoke`. So the old
`can(projectId, 'members.manage')` was the drifted gate (a member with
`members.manage` saw enabled buttons that the middleware would 403), and this commit
removes that drift rather than introducing any.

**The non-admin test is not vacuous.** The mock is `useMe: () => ({ identity:
mocks.identity })` (`PluginsView.test.tsx:14-16`) and the test sets `'member'` (`:123`),
which is a value the real hook actually produces. It then asserts the read-only banner
plus four specific disabled controls. The mock no longer supplies `can`, so it proves
nothing about capabilities — correct, since the component no longer reads them.

**The `pending` union is sound — a stale snapshot cannot fire the wrong action.**
Checked all three hazards from the brief:
- *Wrong action after a refresh*: the discriminant and the target id/key are captured in
  the closed-over `action` before `setPending(null)` (`PluginsView.tsx:366-375`), and both
  removes address the daemon by stable id/key. A target that vanished mid-dialog yields a
  400 in the error panel, never a mutation of the wrong plugin.
- *Stale capability set on `enable`*: fail-safe. `PluginRegistry::activate` requires
  `manifest.capabilities == *approved` (`plugins.rs:544-547`); a stale approved set is
  rejected with 400 "approve the current manifest capabilities before enabling", and
  `build` independently refuses to activate unless `manifest.capabilities` is a subset of
  the approved set (`plugins.rs:325-334`). Under-approval cannot silently grant.
- *Double submit in flight*: impossible by construction rather than by the `disabled`
  prop — Radix's `AlertDialogPrimitive.Action` closes the dialog on click and the handler
  also nulls `pending`, so the button is gone before `busy` is ever read. Every row
  control is disabled on `busy !== null` for the duration. On failure the dialog is
  already closed and `mutationError` renders at page level (`:122`), outside the tabs, so
  it is visible from the Marketplaces tab too.

**A degraded row cannot show a toggle — confirmed.** The `if (!manifest) return`
early-return at `:171-184` renders only the id, the error and Remove; the switch at
`:200` is unreachable for it. Asserted directly by
`expect(screen.queryByRole('switch')).not.toBeInTheDocument()` (`:195`).

**Follow-up 5 (marketplace naming) uses the right key.** `marketplaceTitle` matches on
`entry.key` (`:50`), and both the recommendation's `marketplace`
(`marketplaces.rs:339-344`) and the catalog's `marketplace` (`marketplaces.rs:238`,
`key.clone()`) are the record key, not the alias. `GET /marketplaces` returns disabled
records too, so the lookup still resolves for a marketplace that was just disabled.

**Not checked:** I did not execute a probe for the dual-render MEDIUM finding. It needs a
fixture with `recommended: true, manifest: null, error: <string>` and the reviewer
boundary is strictly read-only, so adding a scratch test to `ui/` was out of scope. The
finding rests on code evidence at both ends — the UI filters at `PluginsView.tsx:110-112`
select the same object, and `api.rs:16048-16056` sets both flags on it — which is
unambiguous, but it is inference rather than an observed render. Residual risk: low, and
one added test case (gap 1 above) settles it either way. No daemon was started, so no
finding here was confirmed against a live `GET /plugins` payload.

**No failures to classify** — the gate had none.

## Open Questions

1. Should a present-but-broken install be marked `recommended` by the daemon at all? The
   MEDIUM dual-render can be fixed in one line in the UI (`!plugin.error`) or one line in
   `api.rs:16048` (`&& snapshot.errors.get(&status.id).is_none()`). The daemon fix also
   cleans up the CLI's view; the UI fix is cheaper. My preference is the daemon, since
   "recommended" currently means two different things.
2. Are the daemon's pseudo-error ids (`legacy-descriptors`, `activation`) meant to be part
   of the `/plugins` contract? If so they deserve a namespace that cannot collide with a
   valid plugin id, and the UI's hardcoded `PLUGIN_ERROR_IDS` set becomes unnecessary.

## Fix Directions

Smallest set that closes the two MEDIUMs, both one-liners in `PluginsView.tsx`:

1. `:325` and `:374` — append `plugins.refresh` to the refresh arrays, matching `:105`
   and `:319`.
2. `:112` — `filter((plugin) => plugin.recommended && !plugin.error)`, or gate
   `status.recommended` on the absence of an error in `api.rs:16048`.
3. `:355` — add "Plugins installed from it stay, but stop receiving updates." to the
   marketplace-remove description.
4. `:43`/`:110` — treat the pseudo-ids as errors only when the id has no installed
   manifest, so a real plugin with that id cannot be hidden.
5. Tests — one fixture with `recommended: true, manifest: null, error: <string>` asserting
   a single rendering; refresh-count assertions on marketplace refresh and remove; a
   Cancel case; one rejected-mutation case asserting the error panel.
