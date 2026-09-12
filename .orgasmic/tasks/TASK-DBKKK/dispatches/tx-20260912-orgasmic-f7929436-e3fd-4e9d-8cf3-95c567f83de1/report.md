# Review TASK-DBKKK — Plugins page (Installed / Browse / Marketplaces)

**Verdict: approve-with-follow-ups.**

Contract (brief item 1) is clean: every route, path-encoding, JSON field name and
`project` query in `ui/src/lib/api.ts` matches the daemon. The approval flow is
correct. All three acceptance criteria are met and independently verified. The
follow-ups are one swallowed error state, two unconfirmed destructive actions,
and a permission-check edge that mismatches the daemon route table.

---

## Findings

### MEDIUM — bug — a broken plugin install is invisible and unremovable
`ui/src/components/PluginsView.tsx:97`

```ts
const installed = (plugins.data ?? []).filter((plugin) => plugin.manifest);
```

`GET /plugins` returns a `PluginStatus` per id drawn from `installed ∪ owners ∪
errors` (`crates/orgasmic-daemon/src/plugins.rs:370-392`). When a directory under
`~/.orgasmic/plugins/<id>` exists but its manifest fails to parse, fails
`validate_id`, has a name/id mismatch, or collides with a core id, reconcile takes
the `Err` arm at `plugins.rs:252` and emits `{ manifest: null, error: "<reason>",
recommended: false }`.

Failure scenario: an install lands a plugin whose `plugin.org` is malformed. The
status row has `manifest === null`, so line 97 drops it from **Installed** and
line 98 drops it from **Recommended**. The Installed tab renders
`EmptyState "No plugins installed."` while the broken plugin sits on disk, its
`error` string never shown, and the Remove button (`:192`) unreachable. The only
recovery is the CLI or deleting the directory by hand. This is exactly the "error
per tab" case brief item 4 asked about.

Fix direction: split on `plugin.manifest` rather than filtering it away — keep
rows where `plugin.manifest || plugin.error` and render a degraded row (id,
`plugin.error` in `text-destructive`, Remove enabled, no version/capabilities/
toggle). Note two pseudo-ids leak through the same channel: `plugins.rs:226`
inserts `"legacy-descriptors"` and `plugins.rs:307` inserts `"activation"` into
the same error map, so `list()` synthesises rows with those ids. Exclude that pair
(or render them as a page-level banner) rather than letting them appear as
plugins named "Legacy Descriptors" and "Activation".

### MEDIUM — bug — Remove is machine-wide, one click, no confirmation
`ui/src/components/PluginsView.tsx:192-197` and `ui/src/components/PluginsView.tsx:296-303`

Plugin Remove posts `/plugins/:id/remove`. The handler
(`crates/orgasmic-daemon/src/api.rs:16113-16140`) collects **every** project root
from the board snapshot and calls `plugins.remove(&id, &roots)`
(`crates/orgasmic-daemon/src/plugins.rs:462-485`), which flips `enabled = false`
in every registered ledger's `.orgasmic/plugins.json` and then `rename`s
`~/.orgasmic/plugins/<id>` into `plugins-removed/`.

Failure scenario: an admin on `projects/alpha/plugins` clicks Remove on a plugin
that is enabled in `beta` and `gamma`. It is torn out of all three. The button is
a bare `variant="destructive"` with no dialog and no copy saying the scope is the
machine, not the project — on a page whose every other control is project-scoped.
Marketplace Remove (`:296`) is the same shape and deletes the clone plus the
record.

Not HIGH: `remove()` keeps a restorable backup at
`plugins-removed/<id>-<uuid>` and the handler reports `data_retained: true`, and
node data is untouched.

Fix direction: reuse the `AlertDialog` already imported at
`PluginsView.tsx:6-14` for the enable flow. Generalise `pendingActivation` into a
`pending` union (`{kind:'enable'|'remove-plugin'|'remove-marketplace', …}`) so
one dialog serves all three, and state the scope in the description: "Removes
<name> from every project on this machine. Node data is kept."

### MEDIUM — correctness — `members.manage` does not match what the daemon enforces
`ui/src/components/PluginsView.tsx:52`

```ts
const canManage = can(projectId, 'members.manage');
```

Every mutation on this page is admin-only **by route-table omission**: none of
`POST /plugins/install`, `/plugins/:id/{activation,update,remove}`,
`POST /marketplaces`, or `/marketplaces/:key/{refresh,activation,remove}` appears
in `MEMBER_ALLOWED_ROUTES` (`crates/orgasmic-daemon/src/api.rs:962-1000`), so
`identity_middleware` rejects a member before the handler's `authz::require` ever
runs.

But `members.manage` is a grantable member capability. `Action::MembersManage` is
in `Action::ALL` and `Action::from_name` parses it
(`crates/orgasmic-daemon/src/authz.rs:40-71`), and `get_me`
(`api.rs:1184-1199`) unions a member's explicit `:ACTIONS:` names onto their role
capabilities. A member with `:ACTIONS: members.manage` in `members.org` therefore
receives `members.manage` from `/me`, `meCan` returns true
(`ui/src/lib/capabilities.ts:8-17`), and the page enables Install, Enable,
Update, Remove, Add marketplace and Refresh — all of which 403.

Not HIGH: no built-in role grants `MembersManage`
(`authz.rs:109-150`), so viewer/editor/artifacts members correctly get the
read-only state and admins correctly get everything. Only the explicit-grant edge
misbehaves, and it fails closed at the daemon — no privilege is leaked, the UI
just lies about what is clickable.

Fix direction: gate on identity, matching the route table:
`const { identity } = useMe(); const canManage = identity === 'admin';`
The existing test mock at `PluginsView.test.tsx:14-16` would move to returning
`{ identity: mocks.canManage ? 'admin' : 'member' }`. If you'd rather keep the
capability shape, add the mutation routes to `MEMBER_ALLOWED_ROUTES` instead —
but that is a daemon change and out of this task's scope.

### LOW — bug — adding a marketplace does not re-light a recommendation
`ui/src/components/PluginsView.tsx:92`

`add()` refreshes `marketplaces` and `catalog`, not `plugins`. A recommendation's
`marketplace` field is filled server-side by `state.marketplaces.offering(&id)`
inside `get_plugins` (`crates/orgasmic-daemon/src/api.rs:16046-16050`).

Failure scenario: the Installed tab shows "Install Meetings" greyed out because
no marketplace offers `meetings` (`marketplace === null`, so `:134` disables and
`install()` at `:84` would no-op anyway). The admin adds the marketplace that
offers it on the Marketplaces tab. The card stays greyed out, with no text saying
why, until a full page reload. Fix: add `plugins.refresh` to the refresh list at
`:92` (and at `:288`, since enabling a marketplace has the same effect). While
there, give the disabled card a reason — "No marketplace offers this plugin" —
instead of a silently dead button.

### LOW — design — recommendation copy hardcodes the official marketplace
`ui/src/components/PluginsView.tsx:129`

> This project has {plugin.id} nodes. Install {title(plugin.id)} from Orgasmic official plugins.

`plugin.marketplace` is whatever key `offering()` returned — the first enabled
marketplace listing that id, official or not. A recommendation sourced from a
user marketplace is described as official. Fix: interpolate the resolved
marketplace's `name ?? alias ?? key` from `marketplaces.data`, falling back to
the key.

### LOW — test — the path-encoded routes have zero assertions
`ui/src/components/__tests__/PluginsView.test.tsx`

Five tests, and the three that assert payloads cover only `/plugins/install`,
`/plugins/calendar/activation` and `/marketplaces`. Nothing exercises the routes
that carry an encoded key or id: `/marketplaces/:key/{refresh,activation,remove}`
and `/plugins/:id/{update,remove}`. Marketplace keys contain slashes
(`marketplace_key` at `crates/orgasmic-core/src/marketplace.rs:143-175` produces
`github.com/theaspirational/orgasmic-plugins`), so encoding is the one place this
surface could silently 404 — and it is the one place untested. Also untested:
Browse-tab install, the search filter, and re-approval.

Fix: one test that clicks Refresh on the Marketplaces tab and asserts
`post` was called with
`'/marketplaces/github.com%2Ftheaspirational%2Forgasmic-plugins/refresh'`, plus
one that seeds `error: 'capabilities grew; enable again to approve them'`,
asserts the "Needs re-approval" badge and the "Re-approve Calendar" label, and
asserts the re-send carries the full grown capability set.

---

## Open Questions

1. **Is `:ACTIONS: members.manage` a configuration you intend to support?** If
   no, the MEDIUM permission finding drops to cosmetic and `identity === 'admin'`
   is simply the clearer expression of the same rule. If yes, the daemon route
   table is the thing that needs to change, not this page.
2. **Should plugin Remove stay machine-wide?** The page is project-scoped
   everywhere else. A project-scoped "Disable here" already exists (the toggle),
   so machine-wide Remove may be the right primitive — it just needs to say so.

---

## Verification Notes

Independent verification, run in the review worktree at `1d415d62`:

| Command | Result |
|---|---|
| `cd ui && npm ci` | rc=0 (`/tmp/dbkkk-npmci.log`) |
| `npm run typecheck` (`tsc --noEmit`) | rc=0 (`/tmp/dbkkk-typecheck.log`) |
| `npm test` | rc=0 — **79 files, 438 tests passed**, incl. `src/components/__tests__/PluginsView.test.tsx (5 tests) 685ms` (`/tmp/dbkkk-test.log:92`) |
| `npm run build` | rc=0, `✓ built in 9.26s` (`/tmp/dbkkk-build.log`) |

No daemon was started. No files were modified.

**Acceptance criteria — all three met:**
- vitest coverage for the page — 5 tests, present and green (gaps noted above, but the criterion is satisfied).
- non-admin read-only — `PluginsView.test.tsx:111-120` sets `canManage=false` and asserts the read-only notice plus Install, the Enable switch, Add marketplace and the marketplace switch are all disabled. In source, all nine mutation controls carry `disabled={!canManage || …}` (`:134, :174, :185, :195, :241, :261, :287, :293, :299`), and `AlertDialogAction` at `:322` is only reachable from an already-gated control.
- `npm run build` passes — verified above.

**Brief item 1 (contract) — checked call by call, no mismatch.**
Compared `ui/src/lib/api.ts:664-744` against the route table at
`crates/orgasmic-daemon/src/api.rs:875-894` and each handler at `:15862-16140`:

- `GET /plugins?project=` → `get_plugins`, `Query<GraphQuery>` reads `q.project`. ✓
- `GET /marketplaces/plugins?project=` → `get_marketplace_plugins`. ✓
- `GET /marketplaces` → `get_marketplaces`. ✓
- `POST /plugins/install {marketplace,id}` → `PluginInstallRequest` (`api.rs:15956-15960`). Exact field match. ✓
- `POST /plugins/:id/update` → `post_plugin_update`, no body. ✓
- `POST /plugins/:id/activation {project,enabled,approved_capabilities}` → `PluginActivationRequest` (`api.rs:16079-16085`). Exact field match; `approved_capabilities` deserialises into `BTreeSet<String>` from a JSON array. ✓
- `POST /plugins/:id/remove` → `post_plugin_remove`, no body. ✓
- `POST /marketplaces {url}` → `MarketplaceAddRequest` (`api.rs:15870-15873`). ✓
- `POST /marketplaces/:key/refresh` / `/remove` → no body. ✓
- `POST /marketplaces/:key/activation {enabled}` → `MarketplaceActivationRequest` (`api.rs:15921-15924`). ✓

Response types match field for field: `MarketplaceStatus`
(`crates/orgasmic-daemon/src/marketplaces.rs:32-41`),
`MarketplacePluginStatus` (`marketplaces.rs:44-53`), `PluginStatus`
(`plugins.rs:74-82`), and the `manifest` subset (`crates/orgasmic-core/src/plugin.rs:19-31`).
`last_refreshed` is seconds (`marketplaces.rs:636-646`), and the UI's
`* 1000` at `PluginsView.tsx:278` is right.

**Key encoding — proven, not inferred.** `marketplace_key` yields keys containing
`/` (`crates/orgasmic-core/src/marketplace.rs:143-175`). Two pieces of evidence
that `encodeURIComponent` is the correct wire form:
- `crates/orgasmic-cli/src/daemon_client.rs:336-346` (`path_segment`) percent-encodes everything outside `A-Za-z0-9-_.~` — a superset of `encodeURIComponent`, and the same result for these keys.
- `crates/orgasmic-daemon/tests/marketplace_routes.rs` hits `/marketplaces/{encoded(key)}/{refresh,remove,activation}` over **real HTTP via reqwest against a bound port** (`request()` at `:145-167`, `encoded()` at `:169-171`), so `%2F` in a `:key` segment is a proven-live path, not an axum assumption.
- Probe: `new URL('/api/marketplaces/' + encodeURIComponent('github.com/theaspirational/orgasmic-plugins') + '/refresh', 'http://127.0.0.1:4848/').pathname` → `/api/marketplaces/github.com%2Ftheaspirational%2Forgasmic-plugins/refresh`. The `%2F` survives the WHATWG URL construction inside `resolveHttpUrl` (`ui/src/lib/transport.ts:91-94`). ✓

**Brief item 2 (approval flow) — correct.** Enabling sends
`plugin.manifest!.capabilities`, the full current manifest set
(`PluginsView.tsx:327`). `activate` requires exact set equality —
`manifest.capabilities == *approved` (`plugins.rs:544-547`) — so the full set is
the only thing that works; a subset would 400. Re-approval: when capabilities
grow, `build()` at `plugins.rs:324-332` sets
`error: "capabilities grew; enable again to approve them"` and leaves the id out
of `active`, so `list()` reports `enabled: false`. The UI's substring probe
`plugin.error?.includes('capabilities grew')` (`:155`) matches that literal,
shows the "Needs re-approval" badge (`:163`), relabels the control "Re-approve"
(`:179`), and routes through the same dialog that re-sends the whole grown set.
Disabling does not drop approvals: the UI sends `approved_capabilities: []`, and
the disable arm at `plugins.rs:576-578` ignores the field entirely
(`activations.entry(id).or_default().enabled = false`), preserving the stored
set. ✓ Brittle but correct — the substring match is coupled to a daemon string
with no shared constant; worth a comment at `:155` pointing at `plugins.rs:331`.

**Brief item 4 (states) — mostly good.** Loading/error/empty are handled per tab
(`:118`, `:214`, `:265`). A marketplace with `error` set still renders, with its
text (`:279`) and an "Unavailable" badge when `!cloned` (`:275`). Browse degrades
correctly when one marketplace is broken: `plugins()` at `marketplaces.rs:214-257`
catches per-marketplace, records the error, and still returns 200 with the
healthy marketplaces' entries. The one gap is the swallowed plugin error, filed
as the first MEDIUM above.

**Brief item 5 (long-running actions) — not an issue.**
`ui/src/lib/transport.ts` has **no** client-side timeout: `requestWithProfile`
(`:149-163`) calls `fetch` with only the caller's optional `signal`, and no
caller here passes one. A real clone cannot be cut off by the UI. (For contrast,
the CLI needs an explicit 300s window — `marketplace_request_timeout` asserted at
`crates/orgasmic-cli/src/daemon_client.rs:741-744`.) Double-click is prevented:
`busy` is a single page-level key (`:58`) and every control carries
`disabled={… || busy !== null}`, so one in-flight mutation freezes all of them.
Coarse — a 2-minute clone locks the whole page — but it is a real pending state
and the spinner at `:137`, `:188`, `:244`, `:262` shows where.

**Brief item 6 (size and reuse) — clean, no finding.** 335 lines is proportionate.
Fetching reuses the existing `useResource` hook (`ui/src/lib/useResource.ts`)
three times; there is no mutation hook in `ui/src/lib` to reuse, and the local
`mutate` helper (`:68-81`) is 13 lines for three concerns (busy key, error,
refresh fan-out) — extracting it would be premature. Every visual element is an
existing primitive: `Tabs`, `Card`, `Badge`, `Button`, `Input`, `AlertDialog`
from `@/components/ui/*`, and `ErrorPanel`/`EmptyState`/`Loading`/`PageHeader`
from `@/components/Primitives`. The only new components are the 7-line
`CapabilityList` and the `title()` slug formatter, neither of which has an
existing equivalent. Nav wiring reuses the established pattern —
`pathForView` (`ui/src/components/AppShell.tsx:119-143`) falls through to its
template literal, so the added union member is all that was needed.

**Not regressions.** The `(!) Some chunks are larger than 500 kB` warning in the
build log is pre-existing (mermaid, cytoscape, shiki grammars), unrelated to this
change.

---

## Fix Directions

Ranked. The first three are the follow-ups worth doing before this page meets a
real broken install or a real member.

1. `PluginsView.tsx:97` — keep error-only statuses and render a degraded row with
   `plugin.error` and an enabled Remove. Exclude the `legacy-descriptors` and
   `activation` pseudo-ids, or surface that pair as a page-level banner.
2. `PluginsView.tsx:192` and `:296` — route both Removes through the
   `AlertDialog` already imported for the enable flow; generalise
   `pendingActivation` into a `pending` union. Say "every project on this
   machine" in the plugin-removal copy, because that is what
   `plugins.rs:462-485` does.
3. `PluginsView.tsx:52` — `const { identity } = useMe(); const canManage =
   identity === 'admin';` to match `MEMBER_ALLOWED_ROUTES`. Update the mock at
   `PluginsView.test.tsx:14-16` to return an identity.
4. `PluginsView.tsx:92` and `:288` — add `plugins.refresh` to both refresh lists;
   give the disabled recommendation card a reason string.
5. `PluginsView.tsx:129` — interpolate the real marketplace name instead of
   hardcoding "Orgasmic official plugins".
6. `PluginsView.test.tsx` — add the encoded-key refresh assertion and a
   re-approval test, per the LOW test finding.
7. `PluginsView.tsx:155` — one comment pointing at `plugins.rs:331`, so the
   substring coupling is visible to whoever next edits that daemon string.
