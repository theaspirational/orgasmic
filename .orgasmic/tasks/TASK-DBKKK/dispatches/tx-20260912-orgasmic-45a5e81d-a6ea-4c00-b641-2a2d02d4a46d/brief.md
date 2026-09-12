# Task 3 of 3: Plugins page in the app

## Context

The daemon now serves marketplace routes (see `crates/orgasmic-daemon/src/api.rs`, routes under `/marketplaces` and `/plugins`, and `PLUGINS-SCOPE.md` section 12). The UI (`ui/`, React, TanStack router in `ui/src/app/router.tsx`, shadcn components, API client in `ui/src/lib/api.ts`) has no plugin management page. Add one.

## Build

Route `projects/$projectId/plugins`, nav label "Plugins", next to Settings. One page, three tabs:

1. **Installed**. Top: a "Recommended" card per entry in `GET /plugins?project=` `recommended` ("This project has meetings nodes. Install Meetings from Orgasmic official plugins" with an Install button). Then every installed plugin: name, version, enabled toggle for this project, capability approval (reuse the existing activation flow and route `POST /plugins/:id/activation`; show the capability list before enabling; grown capabilities show "needs re-approval"), "Update to x.y.z" button when `update_available`, Remove.
2. **Browse**. Every plugin from `GET /marketplaces/plugins?project=`: search box filters by id and description client-side, group by marketplace, "Installed" badge or Install button, version, capabilities on expand.
3. **Marketplaces**. List from `GET /marketplaces`: name, key, Official badge, enabled switch (officials cannot be removed, only disabled), last refreshed, error text when the clone failed, Refresh button, Remove for user ones. An "Add marketplace" input taking a git URL.

Mutations are admin only. Non-admins see the page read-only with buttons disabled and a short hint. Use the existing `can()` helper the shell already uses for `graph.read`; check what permission name the daemon exposes for plugin admin and use it.

Add the typed client calls to `ui/src/lib/api.ts` following its existing style. Loading, empty and error states for each tab. Keep it in one page component plus small sub-components only when a piece is reused twice.

## Tests

Vitest, following `ui/src/components/__tests__/appShellAuthGate.test.tsx` and `genericNodeView.test.tsx` for mocking transport: page renders three tabs, install button calls the route, recommended card appears, non-admin sees disabled buttons, add marketplace posts the URL.

```sh
cd ui && npm ci && npm run typecheck && npm test && npm run build
```

Do not start a daemon. Report the commands and results.

## Scope

Write: `ui/src/**`. No Rust changes; if a route is missing or wrong, stop, describe it in the report, and finalize without it. One commit, then `orgasmic dispatch finalize`.
