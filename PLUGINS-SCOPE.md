# Orgasmic as Lego: plugin architecture scope

Status: exploration only. No code, no ledger mutation. Prepared 2026-09-07 against `d12baf6d`. The nine decisions in section 10 were closed by the user the same day. A review the same day approved the direction and asked for six tightenings; they are folded in and listed in section 10.

Companion: `MEETINGS-IMPLEMENTATION-SPEC.md` is paused. Its open design questions live in `TASK-F98BV`.

Reference: Shi, Zhang, Cui, "A Programming Paradigm for Spatiotemporal Composability" (arXiv 2608.25512), and its runtime Cordis, which runs Koishi's ~4000 community plugins.

## 0. Short version

**Goal.** A node type, its screens, its commands, and its agent skills form one plugin. An agent writes one. You enable it. It works without a rebuild. You share the folder. Dynamic loading is the mechanism. Adding workflows without modifying Orgasmic is the value.

**Where we are.** The daemon already scans a folder of node-type descriptors at boot. It has a type-agnostic node kernel and a generic edit route. The UI has a data-driven editor. But adding a type today means editing about 12 Rust files and 7 TypeScript files, and the UI is compiled into the daemon binary. No plugin concept exists anywhere in the repo.

**What to build, in order.** P0, P1, P2 make a declarative plugin real and prove disable and re-enable. Then P3 with the minimum of P5 for a real meetings workflow. P4 only when something needs a persistent process.

| Slice | One line | Value on its own |
| --- | --- | --- |
| P0 | One registry in the daemon. A descriptor file alone makes a working node type. Compiled types keep their rules through write hooks. | Decisions and glossary become data plugins. Meetings node exists with zero Rust. |
| P1 | UI shell reads the registry. Generic list page and editor for any type. | Meetings gets nav, list, editor with zero TypeScript. |
| P2 | Plugin folders, manifest, enable and disable, ownership and schema rules, scoped plugin principals, `plugin run`, author skill. | Agents can write and share a plugin. Disable and re-enable is proven. |
| P3 | Plugins ship UI code loaded at runtime against a small, explicit SDK. | Meetings workspace UI becomes a plugin. |
| P5 | Core services plugins need: links with anchors, attachments with byte ranges, scoped chat. Minimum first, alongside P3. | The meetings spec's link model, M2, and M5 as shared services. |
| P4 | Persistent sidecar processes. Deferred until a plugin needs one. | Long-running backend logic in any language. |

**Rule of the whole plan.** Use meetings to prove the plugin architecture. Do not finish a general plugin platform first.

**The paper's lessons, applied.** Every SDK registration returns an undo. Plugins declare what they require and what they can use. The whole install is one config tree the loader reconciles. The ledger sits outside the runtime boundary, so removing a plugin never touches data.

## 1. The paper, mapped to Orgasmic

| Paper concept | Meaning | Orgasmic form |
| --- | --- | --- |
| Temporal composability | Remove a component and every side effect reverts. No restart. | Disable a plugin: its routes, nav, node views, event topics, commands, and sidecar go away. Its ledger data stays and renders read-only. |
| `ctx.effect(cb)` returns dispose | One primitive for all mutation. Undo is automatic, LIFO. | Registry writes in Rust and TS both return a disposer. The loader holds them per plugin. Only SDK-managed registrations are tracked. |
| Reactive coeffects, `inject` and `provide` | A component declares needs. It activates when providers exist, deactivates when they leave. | Manifest `REQUIRES` gates the whole plugin. Manifest `OPTIONAL` gates single panes or commands. Meetings requires `core.links`. Its player pane is optional on `core.attachments`. |
| Declarative config tree, entries `{id, url, config, disabled}` | The orchestrator edits a record; the loader does the least disruptive fix. | `~/.orgasmic/user/plugins.org` plus a per-ledger enable list. Change a field, the daemon reconciles. |
| Transactional hot reload | Swap stale modules; roll back if the new code fails to load. | Transactional replacement of SDK-managed registrations only. Not reversal of arbitrary JavaScript. See section 7, P3. |
| System boundary | Files and processes are outside. Undo there is compensation, not reversal. | The ledger is outside. Plugin removal reverts registrations only. Killing a sidecar stops execution; completed writes stay. |
| Capability access control via declared `inject` | A component can only reach what it declared. Reviewable at load time. | Manifest `CAPABILITIES` approved at enable time by an admin. Mediated requests intersect the user, the plugin, and the project. See section 6. |
| Sandboxing untrusted code needs an external boundary | Language checks are not enough. Use a process, wasm, or container with a bridge. | Sidecar plugins are separate processes with scoped daemon tokens but full host filesystem authority. Plugin UI runs same-origin by decision. Wasm is a later upgrade path. |
| Service broker for cross-process calls | Keep the interface, hide the process hop. | Daemon proxies `/api/plugins/<id>/*` to the sidecar. Same auth, same events. |
| Decompose cycles into finer components | Core provides; plugins consume. Avoid mutual dependencies. | Links, attachments, chat scope, and node kernel are core services. Meetings only consumes them. |

## 2. Where Orgasmic is today

### 2.1 Daemon and CLI

Already plugin-shaped:

- Node-type descriptors are files, scanned at boot from `shipped/schema/node-types/` by `NodeTypeRegistry::load` (`crates/orgasmic-daemon/src/node_types.rs:31-47`). A fifth `.org` file is picked up. It drives id minting, dir creation, id to type resolution, submit validation, regenerate prompt, and labels.
- The node kernel is type-agnostic (`crates/orgasmic-core/src/node_kernel.rs`). Comments, journals, and tombstones work on any collection.
- Generic routes exist: `/org/node` get, edit, delete, regenerate, submit. Decisions and glossary already run about 95% on them.
- The incremental indexer accepts any collection dir and ingests its journal (`index.rs:1017-1131`).
- Event payloads carry `layer: String`, so they are type-open (`events.rs:93-105`).
- A `user/` override tier shadows any shipped file (`home.rs:200`). Skills and conventions merge from two tiers.
- `NodeTypeRegistry::resolve` returns a graceful `descriptor: None` for unknown collections. It has zero callers today.
- Members are principals with token hashes and per-project grants (`members.org`). A plugin principal can reuse this shape.

Hardcoded:

- Three parallel enums must change in lockstep: `NodeIdClass` (90 hits), `NodeKind` (64), `NodeLayer` (69, all in `api.rs`). Plus `MintClassArg` in the CLI.
- `NodeLayer::for_id` sends every unknown id to glossary (`api.rs:15838-15852`).
- Projections are a four-way if/else on collection name (`index.rs:1133-1211`). The full scan names four collections and never enumerates `.orgasmic/`. An unknown collection vanishes on restart.
- No generic create endpoint. `POST /graph/nodes` is a stub (`api.rs:852`).
- Event emission for node writes is gated per collection; a new type emits nothing (`api.rs:21606`).
- `repair-ids` walks three collections by name and skips artifacts already.
- `orgasmic update` hardcodes the four descriptor paths in `REQUIRED_RUNTIME_FILES`.
- `reserved_files` is parsed and never read. Dead field.
- Task state set is duplicated: descriptor `:STATES:` and the Rust `LifecycleStage` enum. The enum also carries real guards, such as evidence required on close. Those guards are not duplication.
- No expected-revision check at the writer boundary. Only comment bodies have a compare-and-swap.
- `api.rs` is 45,304 lines. That file absorbs most per-type logic.

### 2.2 UI

Already plugin-shaped:

- `ui/src/components/orgdoc/descriptor.ts` is a real declarative registry. A `NodeDescriptor` is fields bound to org constructs with editor kinds. `NodeDocEditor` renders any descriptor generically.
- `NodeListView` is a generic list shell used by three of four list pages.
- `fetchOrgNode` passes `kind` as a free string to the daemon.
- Theming is pure CSS variables. A plugin styled against `var(--primary)` works in both themes.
- Shared hooks exist for fetch cache, event stream, member capabilities, and refresh bus.
- The artifact block registry is a name to component map with graceful unknown fallback.

Hardcoded:

- Routes are hand-written in `router.tsx`. Nav is a static array in `AppShell.tsx` with three string-literal unions. `ViewName` in `types.ts` and a vite regex repeat the list.
- Node kind union is two members: `'decision' | 'glossary'`. Dispatch is by id prefix in three separate places.
- `DESCRIPTORS` map has two entries. Task and project descriptors are imported by bespoke hosts.
- Detail views are bespoke: `NodeModal` 510 lines, `TaskDialog` 1212, `ArtifactView` 1045.
- Artifact block names are triple-pinned: TS `BLOCK_TYPES`, Rust `BLOCK_TYPES`, and a fixture.
- No CSP anywhere. Runtime `import()` of foreign JS is not blocked today. Adding a loader is the moment to add one.
- The SPA is compiled into the daemon via `include_dir!` (`api.rs:102`). No served path exists outside that bundle.

### 2.3 Distribution and contracts

- Runtime is one tarball per target. `current` symlink swap. `orgasmic update` verifies a required-file list and can roll back.
- Skills link into `~/.agents/skills` and `~/.claude/skills`.
- No API contract document. About 101 routes are typed only by `ui/src/lib/api.ts` and a few wire tests.
- No version negotiation between CLI, daemon, and ledger. `#+orgasmic_version:` is written, never read.
- No first-party notion of plugin, extension, hook, or pack exists in the repo.

### 2.4 Auth

- Roles `viewer`, `editor`, `artifacts`. Admin bypasses. Eleven actions through one seam, `authz::require`.
- Grants are re-read per request, so revoke is immediate.
- Agent sandbox is four coarse switches. Not a plugin boundary.

## 3. Target shape

```text
┌──────────────────────────── orgasmic core (the "context") ────────────────────────────┐
│ node kernel · writer/tx/idempotency · registry + write hooks · index (generic)        │
│ links with anchors (new) · attachments (new) · authz/auth/members · events bus        │
│ run supervisor + drivers · plugin loader: config tree, reconciliation, principals,    │
│ sidecar supervision, UI asset serving · UI shell: router, nav, dock, primitives, SDK  │
└──────┬───────────────┬──────────────────┬──────────────────┬──────────────────────────┘
       │ provides      │ provides         │ provides         │ provides
   core.nodes      core.links      core.attachments     core.chat / core.events / core.authz
       ▲               ▲                  ▲                  ▲
       │ requires      │                  │ optional         │
┌──────┴───────┐ ┌─────┴────────┐ ┌───────┴──────┐ ┌─────────┴────────┐ ┌──────────────┐
│ decisions    │ │ glossary     │ │ tasks        │ │ artifacts        │ │ meetings     │
│ tier A       │ │ tier A       │ │ tier A + C1  │ │ tier A + C1      │ │ tier A + B   │
│ (no code)    │ │ (no code)    │ │ (compiled)   │ │ (compiled)       │ │ (user dir)   │
└──────────────┘ └──────────────┘ └──────────────┘ └──────────────────┘ └──────────────┘
```

### 3.1 Three plugin tiers

**Tier A, declarative.** A folder with a manifest and no executable module. Node-type descriptor, editor fields, list columns, nav entry, edge kinds from the closed vocabulary, skills, conventions, prompt parts. The daemon serves it through the registry and the generic node routes. The UI renders the generic list and editor. An agent writes one in minutes. This is the Lego brick. Not "safe": its skills and prompts steer privileged agents. Enabling one is still a trust decision.

**Tier B, UI code.** Adds a JS module built against a small, explicit `@orgasmic/plugin-sdk`. The daemon serves it from the plugin folder. The shell exposes a registry for routes, nav, node views by collection, dock panes, and artifact blocks. Each registration returns a disposer. Runs in the app origin with the user's session, so it holds the user's full application authority, not only its manifest capabilities. No iframe mode.

**Tier C, backend code.** Two forms:

- C1, compiled in. A Rust crate per built-in type registers through the same registry as tier A, and registers write hooks. Generic writes route through those hooks, so a task close still needs evidence. Not dynamic. This is how tasks and artifacts become honest plugins without dlopen.
- C2, sidecar. The plugin ships an executable in any language. The daemon supervises it like the existing provider host, talks JSON-RPC over stdio, proxies `/api/plugins/<id>/*`, forwards events, and hands it a token scoped to approved capabilities. The token bounds daemon calls. The process keeps full host filesystem authority. Kill the process group and execution stops; completed writes stay.

Before C2 exists, a plugin can ship one-shot executables run by `orgasmic plugin run <id> <command>` with the same scoped token. That covers importers and media jobs without a persistent process.

Wasm in-process is a later option. Real sandbox, real undo, heavy toolchain. Not now.

### 3.2 What stays core, and why

The paper's rule: providers in core, consumers in plugins, no cycles. Anything two or more plugins need is core.

- Node kernel, writer, index, events, authz, run supervisor. Already core.
- **Node links with anchors.** A reserved filename gives no behavior. Core owns the relationship record: source, target, edge kind from the closed vocabulary, an anchors payload, revision, attribution. Core validates the payload against a schema the plugin declares, writes it transactionally with the node, checks expected revision, indexes backlinks, and emits events. The plugin owns the meaning and display of recording timestamps. This is `core.links`.
- Attachments and byte-range media. Meetings needs it. Artifact images need it. Any document plugin needs it.
- Scoped chat context. Meetings needs it. Any node-scoped chat needs it.
- Plugin loader, principals, and SDK. Core by definition.

Meeting-specific logic stays in the plugin: transcript cues, alignment segments, workspace layout, whisper and ffmpeg orchestration.

## 4. Manifest sketch

Keep org. The registry already parses it and the house style is org everywhere.

```org
#+title: meetings
* Plugin
:PROPERTIES:
:ID: meetings
:VERSION: 0.1.0
:PLUGIN_API: 1
:SCHEMA: 1
:REQUIRES: core.nodes@1 core.links@1
:OPTIONAL: core.attachments@1 core.chat@1
:PROVIDES: nodes.meetings
:CAPABILITIES: links.write attachments.write sessions.launch
:UI: ui/index.js
:COMMANDS: import
:END:
** Node type meetings
:PROPERTIES:
:ID_PREFIX: MEET-
:COLLECTION: meetings
:LABEL: Meeting
:LABEL_PLURAL: Meetings
:STATES: active archived
:EDGES: RELATES_TO PRODUCES
:ANCHOR_SCHEMA: schema/anchor.json
:RESERVED_FILES: alignment.org
:END:
** View
:PROPERTIES:
:NAV: Meetings
:LIST_COLUMNS: title occurred_at state
:EDITOR: title tags body section:Notes property:OCCURRED_AT
:END:
** Pane player
:PROPERTIES:
:NEEDS: core.attachments
:END:
** Skill meetings
```

- `PLUGIN_API` is the loader contract version. The daemon refuses a manifest it cannot serve and says why.
- `SCHEMA` is the stored data version. Every node the plugin writes carries `#+orgasmic_plugin: meetings@1`.
- `REQUIRES` gates the whole plugin. `OPTIONAL` plus a pane-level `NEEDS` gates one pane or command. The player pane deactivates when attachments are unavailable. The node stays readable.
- `COMMANDS` names executables under `bin/`, run by `orgasmic plugin run meetings import`.

## 5. Lifecycle

| Step | Command | What happens |
| --- | --- | --- |
| Add | `orgasmic plugin add <path or git url>` | Copies into `~/.orgasmic/user/plugins/<id>/`. Runs check. Not yet active. |
| Check | `orgasmic plugin check <id>` | Validates manifest, descriptor, edge kinds against the closed vocabulary, anchor schema, capabilities known, UI bundle imports only the SDK. |
| Enable | `orgasmic plugin enable <id> [--project P]` | Admin only, the `members.manage` authority. Shows requested capabilities and asks for approval. Rejects a collection or prefix another plugin owns. Writes the config tree entry. Loader registers type, views, routes, commands, skill. |
| Disable | `orgasmic plugin disable <id>` | Loader runs the disposers. Nav gone, routes 404, commands refused, sidecar process group killed. Ledger data stays and renders read-only. |
| Remove | `orgasmic plugin remove <id>` | Deletes the plugin folder. Never deletes `.orgasmic/<collection>/`. Ownership record stays so the data is still identified. |
| Update | edit files or `plugin add` again | Dir watcher reconciles. If the new manifest adds capabilities, the plugin is disabled until an admin re-approves. If `SCHEMA` changes, see 5.1. |
| Share | git repo or tarball of the folder | Nothing else. A community index is not needed yet. |

Per-ledger enablement is organizational scoping. It is not isolation from trusted same-origin code.

### 5.1 Data ownership and versions

Files surviving is necessary, not sufficient. Core must still identify and safely show the data.

- **Ownership.** Core keeps a registry of collection and id prefix to plugin id. Enable rejects a collision by name. Remove keeps the record.
- **Unavailable plugin.** Nodes in an owned collection render read-only through the generic view: title, properties, body, journal. Writes are refused with an error naming the plugin.
- **Stored schema.** Every node the plugin writes carries `#+orgasmic_plugin: <id>@<schema>`. On load, core compares it to the enabled plugin's `SCHEMA`.
- **Mismatch.** Older or newer stored schema than the plugin supports: read-only, writes refused by name, no silent rewrite. A plugin may declare `:SCHEMA_ACCEPTS: 1 2` to read older versions it understands.
- **No migration framework in v1.** Refusing unsupported changes is enough.

### 5.2 Agent authoring loop

`orgasmic plugin scaffold <id>` creates the folder from a template. A shipped skill, `orgasmic-plugin-author`, explains the manifest, the SDK, the command contract, and the check command. The agent writes, runs check, enables, and sees the result live.

## 6. Trust and permissions

Say what each tier can reach. Never imply a boundary that is not there.

- **Tier A** has no executable module. It is not safe by default. Its skills and prompts influence privileged agents. Enabling one is a trust decision, like installing a skill.
- **Tier B** runs in the app origin with the user's session. It holds the current user's full application authority, not only its manifest capabilities. Capabilities on a tier B manifest are documentation and a check input, not a fence. There is no iframe mode. Enable only plugins you or your agent wrote, or that you have read.
- **Tier C2 and `plugin run`** get a plugin principal: a member-shaped token whose grants are the approved capabilities, scoped to the enabled projects. It bounds daemon calls only. The process holds host filesystem authority. The enable prompt says so.
- **Mediated requests.** A call that reaches the daemon through a plugin route or proxy carries two principals: the initiating user and the plugin. Effective authority is the intersection of the user's grants, the plugin's approved capabilities, and the enabled project. A viewer cannot gain write access by calling a privileged plugin route.
- **Who enables.** Admin, through `members.manage`. Per-ledger activation is scoping, not isolation.
- **Capability growth.** An update that adds capabilities disables the plugin until re-approved.
- **CSP.** Add one the day the UI loader lands. Allow `self` and `/plugins/*` only.

## 7. Delivery plan

Sequence: P0, P1, P2 first. Then P3 with the minimum of P5. P4 when a plugin needs a persistent process.

### P0, one registry in the daemon

Collapse `NodeIdClass`, `NodeKind`, `NodeLayer`, and `MintClassArg` into registry-backed lookups. Wire `NodeTypeRegistry::resolve` as the caller so unknown ids stop falling into glossary. Full scan enumerates `.orgasmic/*` collections. Generic projection for any collection from descriptor fields. Generic `POST /org/node` create, replacing the stub. Events emit for any collection. `repair-ids` walks all collections. Registry loads through `resolve_loader` so the `user/` tier and `user/plugins/*/` count. `update.rs` stops hardcoding four descriptors. Wire or delete `reserved_files`. Add `PLUGIN_API`.

Write hooks: a compiled type registers `validate_write` and `validate_transition` with the registry. The generic `/org/node` edit and the generic create call them. Task guards, such as evidence required on close, keep working on every path. The descriptor `:STATES:` becomes the single source for the state set; `LifecycleStage` stays for behavior and a test asserts the two agree.

Scope guard: eliminate duplicated authority. Do not rewrite the task engine.

Exit: drop a `meetings.org` descriptor in the user tier. Create, list, edit, index, reindex, restart, and CLI all work with zero Rust edits. A `MEET-` id never reaches the glossary writer. A generic write cannot close a task without evidence.

Size: largest slice. About 220 enum hits, most in `api.rs` and `index.rs`. Needs a review gate.

### P1, UI registry and declarative views

Shell registry for routes, nav, node view by collection, and capability gating, fed by `GET /api/plugins`. `NodeDescriptor` becomes data delivered from the manifest. Generic list page and generic detail modal for any collection. Read-only rendering for unavailable or schema-mismatched collections. Drop the string-literal unions and the prefix dispatch.

Exit: the meetings descriptor yields a nav entry, list page, and editor with zero TypeScript edits. Disable the plugin and the same nodes render read-only.

### P2, plugin loader and packaging

Manifest format. `~/.orgasmic/user/plugins/<id>/`. Per-ledger enable list. `orgasmic plugin add | check | enable | disable | remove | list | scaffold | run`. Reconciliation on change. Ownership registry and collision rejection. Stored schema header and mismatch refusal. Plugin principals with scoped tokens, reusing the member token shape. Admin-only enable with capability approval and re-approval on growth. `plugin run <id> <command>` executes `bin/<command>` with the daemon URL and the plugin token in the environment. Author skill. Move the four built-in descriptors to `shipped/plugins/<id>/plugin.org` so built-ins and user plugins share one loader.

Exit: add and enable a meetings plugin from a folder. Disable removes nav, routes, and commands. Data intact and readable. Re-enable restores everything. A second plugin claiming `MEET-` is rejected. A viewer calling `plugin run` cannot write.

### P3, UI code plugins

Serve `/plugins/<id>/ui/*` from the plugin folder. Publish `@orgasmic/plugin-sdk` as a deliberately selected subset: the node, links, attachments, and events client functions and their types from `ui/src/lib/api.ts` and `ui/src/lib/types`; the shadcn primitives; `useResource`, `useEventStream`, `useMe`, `useActiveProject`; transport `get` and `post`. Nothing else is exported. The host React instance is shared through an import map so the plugin bundle never bundles its own React. Styles: plugin CSS is scoped under `[data-plugin=<id>]` and uses the host tokens. Project and node context arrive through `ctx`. Compatibility: `PLUGIN_API` plus the SDK's own semver in the manifest. `register(ctx)` returns a disposer. Introduce CSP.

Hot reload, narrowed: transactional replacement of SDK-managed registrations only. Registration is staged: the new module registers into a shadow set, and the loader swaps atomically or discards the shadow set. Cleanup is owned: every subscription, timer, and pending request a plugin opens goes through `ctx` so the disposer can close it. Side effects run during import are not tracked and must not exist; check refuses top-level effects it can detect. Unsaved drafts live in host-owned state keyed by node id, outside the replaceable view, so a reload never loses them. Tests cover activation that fails halfway.

Exit: a plugin-provided React view replaces the generic editor for meetings. A broken save keeps the old view running. A draft survives a reload.

### P5, core services

Minimum first, alongside P3, then the rest.

- `core.links`: link record with anchors, plugin-declared anchor schema, transactional write with expected revision, backlink index, events, generic backlink display in P1's views.
- `core.attachments`: asset store with immutable revisions, resumable upload, byte-range routes, media grants for bearer profiles.
- `core.chat`: scoped chat context and durable conversation identity.

These are the meetings spec's link model, M2, and M5.

Exit: a meeting links a task at a timestamp, the task shows the backlink, the recording plays with seek, and the chat is meeting-scoped.

### P4, sidecar plugins

Deferred until a plugin needs a persistent process. Manifest `SIDECAR`. Daemon supervises with the existing run supervisor pattern, in its own process group. JSON-RPC over stdio, reusing the existing jsonrpc mode. Proxy `/api/plugins/<id>/*` with the two-principal intersection. Forward events. Kill the process group on disable. Completed writes are not reverted; say so.

Exit: a TypeScript sidecar adds a route and serves it. Disable kills it and the route 404s.

## 8. Built-ins as plugins

| Type | Verdict | Why |
| --- | --- | --- |
| decision | Tier A after P0 and P1 | Already 95% on generic routes. PARENT and SUPERSEDES tree needs one descriptor-declared edge rule. |
| glossary | Tier A after P0 and P1 | Same. `RELATES_TO` projection becomes generic edge indexing through `core.links`. |
| task | Tier A descriptor plus C1 behavior | Lifecycle guards, dispatch stages, subtasks, comments, board. Behavior stays compiled and registers write hooks. `LifecycleStage` stays; the descriptor owns the state set and a test keeps them equal. |
| artifact | Tier A descriptor plus C1 behavior | Versions, MDX generation, block registry, comments. Same treatment. Block registry becomes plugin-open. |
| project, goal, handoff | Stay core | Singletons, not collections. |

Extracting tasks or artifacts to sidecars is possible later and has no payoff now.

## 9. Meetings under this architecture

The meetings spec survives. It re-slices:

- Meeting node: tier A descriptor. P0 and P1.
- Links, anchors, backlinks: `core.links` with a meetings anchor schema. P5 minimum.
- Attachments and playback: `core.attachments`. P5.
- Workspace UI: tier B plugin. P3.
- Media operations: agent runs plus CLI attach, as recommended in `TASK-F98BV` Q6. Whisper and ffmpeg jobs as `plugin run meetings <command>`. No persistent sidecar needed.
- Chat scope: `core.chat`. P5.
- Max import: `plugin run meetings import`, idempotent, against the public API.

The spec's acceptance matrix still applies. Section 9 of the spec, the source map, is replaced by this document's P0 to P5.

## 10. Decisions

Closed by the user on 2026-09-07. Eight follow the recommendation. Number 3 does not.

| # | Decision | Chosen |
| --- | --- | --- |
| 1 | Plugin install scope | Per user home under `~/.orgasmic/user/plugins/<id>/`, enabled per ledger. |
| 2 | Manifest format | Org. Same parser as the node-type descriptors. |
| 3 | Tier B trust | Always same-origin. No iframe mode, not even for community plugins. Enabling a plugin is the trust decision. |
| 4 | Sidecar SDK language | TypeScript first. Same esbuild toolchain as the provider host. |
| 5 | Tasks and artifacts | Stay compiled in as C1. They register through the registry. No sidecar extraction. |
| 6 | Hot reload | Yes, in P3, for plugin UI, narrowed to SDK-managed registrations. Sidecars restart. |
| 7 | API contract | A selected subset of the existing TypeScript client types becomes the SDK. No OpenAPI. |
| 8 | Meetings first slice | P0 and P1 first. Meetings is the first plugin. No hardcoded fifth type. |
| 9 | Sharing | Git URL or tarball only. No index. |

Review tightenings, same day, all adopted:

1. Links with anchors are a core service, `core.links`, not a reserved filename. Section 3.2, P5.
2. Node discovery is generic; task behavior is not. Write hooks in P0. `LifecycleStage` stays. Section 7.
3. Hot reload promises transactional replacement of SDK-managed registrations only. Staged registration, owned cleanup, drafts outside the view, halfway-failure tests. Sidecar kill stops execution and nothing more. Section 7, P3 and P4.
4. Trust language is precise per tier. Two-principal intersection on mediated requests. Admin-only enable. Re-approval on capability growth. Section 6.
5. Data survives with meaning: ownership registry, collision rejection, read-only fallback, stored schema header, refusal instead of rewrite. Section 5.1.
6. The SDK is a selected subset. Commands dispatch through `orgasmic plugin run <id> <command>`. Section 7, P3 and P2.
7. Sequencing: P0, P1, P2, then P3 with minimum P5, then P4 on demand. Manifest distinguishes `REQUIRES` from `OPTIONAL`.

## 11. Not in scope

- A plugin marketplace or index.
- Iframe isolation for plugin UI.
- Wasm or container isolation for sidecars.
- An OpenAPI spec.
- A schema migration framework. Refusal is the v1 policy.
- Reversal of arbitrary JavaScript or of completed sidecar writes on reload or disable.
- Dynamic loading of Rust code.
- Rewriting the task engine, or `api.rs` beyond what P0 needs to remove the per-type lookups.
- Cross-project plugins or cross-project node links.
- Keystroke-level collaborative editing.
