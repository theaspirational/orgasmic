# Review TASK-DBKKK: Plugins page in the app

One commit over `main` (`git diff main...HEAD`), UI only. Implementer was codex gpt-5.6-sol. The brief asked for a project route `projects/$projectId/plugins` with three tabs: Installed (recommended cards from `GET /plugins?project=` `recommended`, enable toggle with capability approval and re-approval via `POST /plugins/:id/activation`, update, remove), Browse (`GET /marketplaces/plugins?project=`, client-side search, install), Marketplaces (`GET /marketplaces`, add by URL, refresh, enable/disable, remove for user ones only). Admin-only mutations; non-admins read-only. Typed client calls in `ui/src/lib/api.ts`. Vitest coverage.

Check, in order:

1. **Contract match.** Compare every call in `ui/src/lib/api.ts` against the daemon routes and payloads in `crates/orgasmic-daemon/src/api.rs` and `marketplaces.rs` (route paths, key encoding for `/marketplaces/:key/...`, JSON field names, `project` query). A mismatch that would 400 or 404 at runtime is blocking.
2. **Approval flow.** Enabling sends the approved capability list the daemon expects; a grown capability set shows "needs re-approval" and re-sends the full set. Disabling does not silently drop approvals.
3. **Permission gate.** Which permission the page checks for admin (`members.manage`?) and whether it matches what the daemon enforces. Non-admin state must disable every mutation control, not just some.
4. **States.** Loading, empty, error per tab. A marketplace with `error` set still renders with its error text. Browse degrades when one marketplace is broken.
5. **Long-running actions.** Add marketplace, refresh, install and update can take a long time (real clone). The UI transport timeout in `ui/src/lib/transport.ts`: does it cut these off? Is there a pending state so the user does not double-click install?
6. **Size and reuse.** `PluginsView.tsx` is 335 lines. Flag duplicated fetch/mutation boilerplate that an existing hook in `ui/src/lib` already covers. Flag any new component that duplicates an existing shadcn primitive.
7. **Tests.** `PluginsView.test.tsx` asserts real routes and payloads, not just rendering.
8. Run `cd ui && npm ci && npm run typecheck && npm test && npm run build`. No daemon.

Verdict: approve, approve-with-follow-ups, or reject, each finding with file and line and a concrete fix. Item 1 mismatches are blocking. Finalize with `orgasmic dispatch finalize`, verdict in the summary.
