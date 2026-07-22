# orgasmic UI

Operator workbench for the orgasmic daemon. React 19 + TanStack Router +
Tailwind/shadcn, shipped both as a browser SPA and as a Tauri desktop app that
bundles and supervises a local daemon.

## Transport seam

All UI ⇄ daemon traffic goes through a single seam — nothing else calls `fetch`
or `WebSocket` directly. The seam prepends the `/api` prefix to every daemon
route, attaches the active profile's bearer token, and rewrites `http(s)` → `ws(s)`
for the event stream.

| Module | Role |
|--------|------|
| `src/lib/transport.ts` | The seam. Adds `/api`, bearer header, WS token; surfaces `HttpError`. |
| `src/lib/api.ts` | Typed wrappers for every daemon endpoint. |
| `src/lib/useResource.ts` | Suspense-free fetch hook used by views. |
| `src/hooks/useEventStream.ts` | Shared WebSocket with exponential-backoff reconnect. |
| `src/lib/backend.ts` | Backend profiles (base URL + token), Tauri local-daemon detection. |
| `src/lib/routing.ts`, `theme.ts`, `storage.ts`, `timers.ts` | URL/query, theme, localStorage, timer seams. |

Source modules carry `@arch arch_MK2Q2.*` annotations linking them back to the
architecture graph.

## Screens

Routing is path-based (TanStack Router). `/` redirects to the last project's
last view; `/board` is cross-project. Project screens live under
`/projects/:id/…`. Paths below are logical — the transport adds `/api`.

| Route | View | Key daemon endpoints |
|-------|------|----------------------|
| `/board` | Board (all projects) | `GET /board` |
| `/projects/:id` · `…/decisions` | Decisions | `GET /decisions` |
| `…/project` | Project overview | `GET /projects/:id`, `GET /projects/:id/tasks` |
| `…/tasks` (`?task=`, `?layout=list\|kanban`) | Tasks + Task dialog | `GET /projects/:id/tasks`, `GET /projects/:id/tasks/:taskId`, `GET /tasks/:taskId/activity`, `POST /tasks/:taskId/comments`, `POST /tasks/:taskId/subtasks` |
| `…/architecture` | Architecture | `GET /architecture`, `GET /architecture/nodes` |
| `…/glossary` | Glossary | `GET /glossary` |
| `…/graph` | Graph | `GET /graph/nodes`, `/graph/edges`, `/graph/layout` (+ `PATCH`), `/graph/parse-errors` |
| `…/activity` | Activity | `GET /tasks/:taskId/activity` |
| `…/runs` | Runs | `GET /runs`, `GET /runs/:id`, `POST /runs/:id/{recover,release,input,runtime-options}` |
| `…/prompts` | Prompt Studio | `GET/POST /prompt-specs/*`, `/prompt-specs/parts/*`, `/prompt-specs/context-packs` |
| `…/org` | Org editor | `GET /org/file`, `GET /org/node`, `POST /org/file`, `POST /org/node/:id/edit` |
| `…/status` | Status | `GET /daemon/status`, `/recovery/status`, `/graph/parse-errors`, `/auth/whoami` |
| `…/settings` | Settings | backend profiles, theme, `GET /healthz` |

The **Manager** is a root-level overlay rather than a route, sized via the
`?manager=peek\|workbench\|focus` search param (RunDock → ManagerWorkbench). It
drives `GET /manager/state`, `POST /manager/launch`, `GET /managers/drivers`,
`GET /skills`, and `GET/POST /tx`, with a live
terminal pane over xterm. Pipeline stages post to `/grill`, `/architect`, and
`/plan`. Adding a project browses `GET /filesystem/{roots,entries}` and
`POST /filesystem/validate-project`.

## Events

`GET /api/ws` carries daemon topics (board/task/run/manager/graph/daemon). The
shared socket reconnects with exponential backoff (1s → 15s); panels refresh
when their topic arrives. Because browser `WebSocket` cannot set an
`Authorization` header, the seam appends the profile token as `/api/ws?token=…`;
the daemon accepts that query token on the WebSocket route only.

## Backend profiles & auth

**Settings → Backend** manages one or more profiles (base URL + bearer token);
the active profile drives the transport. A built-in **Local daemon** profile
points at the current origin. In the Tauri desktop build the local profile is
populated automatically from the bundled daemon (origin + token), which listens
on `http://127.0.0.1:4848`.

## Development

```bash
cd ui
npm install
export ORGASMIC_DEV_TOKEN="$(cat "$ORGASMIC_HOME/user/auth/token")"  # optional but recommended
npm run dev   # vite on 127.0.0.1
```

The dev server proxies `^/api` (HTTP + WebSocket upgrades) to the daemon at
`http://127.0.0.1:8739` — override with `ORGASMIC_DAEMON_URL`. When
`ORGASMIC_DEV_TOKEN` is set, the proxy injects `Authorization: Bearer …` on both.
A navigation-fallback middleware serves `index.html` for SPA routes, and
`ORGASMIC_UI_BASE_PATH` sets the app base path.

When hitting a daemon directly (no proxy), paste the token into a Settings
backend profile instead.

```bash
npm run typecheck
npm run test       # vitest
npm run build      # tsc --noEmit && vite build
npm run preview
```

## Desktop (Tauri)

The same SPA ships as a Tauri app that locates the `orgasmic` CLI, launches and
supervises a local daemon, and supports channel-based self-update.

```bash
npm run tauri:dev          # desktop dev
npm run tauri:build        # desktop bundle
npm run tauri:bundle:mac   # macOS .app + .dmg
npm run tauri:android:dev  # Android emulator (host 10.0.2.2)
```

A small **bootstrap shell** (`bootstrap.html` / `src/bootstrap.ts`, built with
`npm run build:bootstrap`) is shown first: it probes for the CLI via the
`runtime_probe` command, then redirects to the daemon UI URL, or explains how to
install the CLI if it is missing.
