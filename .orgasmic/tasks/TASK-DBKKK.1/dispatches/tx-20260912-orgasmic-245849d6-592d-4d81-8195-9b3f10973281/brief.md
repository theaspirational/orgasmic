# Review TASK-DBKKK.1: Plugins page follow-ups

One commit over `main` (`git diff main...HEAD`), two UI files. Implementer was codex gpt-5.6-sol. It closes the six follow-ups from the first review of TASK-DBKKK:

1. Rows with `manifest: null` and `error` render degraded with Remove; `legacy-descriptors` and `activation` pseudo-ids become one page-level alert, not plugin rows.
2. Plugin and marketplace Remove confirm via the existing `AlertDialog` with scope copy ("every project on this machine. Node data is kept.").
3. Admin gate is `useMe().identity === 'admin'`.
4. Marketplace add and enable/disable refresh `plugins`; a null-marketplace recommendation says "No marketplace offers this plugin."
5. Recommendation copy names the offering marketplace (`name ?? alias ?? key`).
6. Tests for path-encoded refresh and update routes, grown-capability re-approval, Browse install, search, degraded row, confirm dialogs.

Verify each against the diff. Then check the `pending` dialog union: a stale `pending` cannot fire the wrong action after the list refreshes; the confirm button is disabled while the request is in flight; a failed request shows its error and closes or keeps the dialog sanely. Confirm the degraded row cannot show a toggle. Confirm the `identity` mock does not silently make the non-admin test vacuous.

Run: `cd ui && npm ci && npm run typecheck && npm test && npm run build`. No daemon.

Verdict: approve, approve-with-follow-ups, or reject. Finalize with `orgasmic dispatch finalize`, verdict in the summary.
