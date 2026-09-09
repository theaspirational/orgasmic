---
name: orgasmic-plugin-author
description: 'Author or update an Orgasmic workflow plugin: its plugin.org manifest, node collection, and scoped CLI commands. Use when asked to build, validate, enable, or run an Orgasmic plugin, not a Codex plugin.'
---

# Author an Orgasmic plugin

The `orgasmic plugin` family provides `orgasmic plugin scaffold`,
`orgasmic plugin add`, `orgasmic plugin check`, `orgasmic plugin enable`,
`orgasmic plugin disable`, `orgasmic plugin remove`, `orgasmic plugin list`,
and `orgasmic plugin run`.

Use the existing node API and CLI; do not write ledger files directly. Check
`orgasmic plugin --help` and the relevant leaf command's `--help` against the
installed runtime before executing. The foundation supports declarative
collections, commands, same-origin UI views with hot reload, links, attachments,
and chat (see "Chat" below). Sidecars remain unavailable.

## Build and verify

1. Run `orgasmic plugin scaffold meetings`. It creates a disabled folder at
   `~/.orgasmic/user/plugins/meetings` (under `ORGASMIC_HOME` when overridden).
   Edit its `plugin.org`, using the shape below. Pick an unused collection and
   prefix; ownership survives removal and cannot be claimed by another plugin.
2. For each declared command, create an executable `bin/<command>` inside that
   folder. No symlinks or paths escaping the plugin folder. For this example,
   `bin/create` can contain the shell snippet below; make it executable.
3. Run `orgasmic plugin check meetings`. For an external development folder use
   `orgasmic plugin check --path /absolute/plugin-folder`, then
   `orgasmic plugin add /absolute/plugin-folder`. A Git URL is also accepted;
   installation alone never enables a plugin.
4. Ask the user to approve the displayed capabilities, then run
   `orgasmic plugin enable meetings --project PROJECT`. Only use `--yes` after
   explicit approval. Inspect with `orgasmic plugin list --project PROJECT`.
5. Run `orgasmic plugin run meetings create --project PROJECT`. Arguments after
   `--` go to the child command. Verify its node in the collection's generic
   list/editor. Edit, check, and run again; descriptor
   changes reconcile without restarting the daemon.
6. Test `orgasmic plugin disable meetings --project PROJECT`: retained nodes
   must still be readable, but not writable. Re-enable to resume. Removal with
   `orgasmic plugin remove meetings` disables it everywhere and moves code to
   recovery storage; it does not delete nodes or release ownership.

## Manifest shape

One top-level `Plugin`, at most one nested node type. The folder name must match
`ID`. Version is numeric major.minor.patch; schema numbers are positive.
`COMMANDS`, `SCHEMA_ACCEPTS`, states, and transitions are optional. Currently
services are `core.nodes@1`, `core.links@1`, `core.attachments@1`, and
`core.chat@1`; list each in `REQUIRES`, or in `OPTIONAL` when the plugin also
works on a host without it. Capabilities
are `nodes.read/write`, `links.read/write`, `attachments.read/write` (spell out
each string, not the slash shorthand), and implicit `ui.execute`.

```org
* Plugin
:PROPERTIES:
:ID: meetings
:VERSION: 0.1.0
:PLUGIN_API: 1
:SCHEMA: 1
:SCHEMA_ACCEPTS: 1
:REQUIRES: core.nodes@1
:CAPABILITIES: nodes.read nodes.write
:COMMANDS: create
:END:
** Node type meetings
:PROPERTIES:
:COLLECTION: meetings
:ID_PREFIX: MEET-
:LABEL: Meeting
:LABEL_PLURAL: Meetings
:REQUIRED_PROPERTIES: ID
:STATES: active archived
:TRANSITIONS: active>archived archived>active
:END:
```

```sh
#!/bin/sh
exec orgasmic node create --kind meetings --title 'Planning' --project "$ORGASMIC_PROJECT"
```

## Two rules that bite

- **Preserve the schema header.** Core creates `#+plugin: meetings schema=1`
  before the first heading, alongside the custom-state `#+todo:` header. Nodes
  missing the plugin header, belonging to another plugin, or carrying a schema
  outside `SCHEMA_ACCEPTS` are read-only. A title edit must not remove either
  header. Raising `SCHEMA` does not migrate existing nodes: declare old schemas
  accepted only when the new code genuinely supports them; otherwise stop and
  plan migration. Do not bypass the gate by hand-editing ledger state.
- **New capabilities need re-approval.** Increasing `CAPABILITIES` makes the
  plugin unavailable until an admin explicitly enables it with the current
  capability set. Manifest changes invalidate existing run tokens. Never
  silently approve, reuse stale credentials, or fall back to an admin token.

`plugin run` supplies `ORGASMIC_DAEMON_TOKEN`, `ORGASMIC_PLUGIN_TOKEN`,
`ORGASMIC_DAEMON_URL`, `ORGASMIC_PLUGIN_ID`, and `ORGASMIC_PROJECT`. Child CLI
calls inherit the plugin principal: capabilities intersect with the caller's
role, writes stay within owned collections, and calls stay in one project.
Do not log tokens. Disable, member revocation, or manifest changes revoke them;
normal command exit revokes its lease too. Commands still run as the OS user
with full host filesystem access: this is daemon authorization, not a sandbox.

## UI views and hot reload

Add `:UI: ui/index.js` and `:SDK: ^1.0` to the root drawer. Check requires that
entry file inside the plugin folder. UI automatically adds `ui.execute` to the
approval set: existing declarative approvals cannot silently enable executable
code. It acknowledges full application-session authority, not a sandbox.

The authenticated route is `/plugins/<id>/ui/<project>/index.js`; keeping the
project in the path also scopes relative chunk imports. Only JavaScript and CSS
assets are served today; fonts, images, JSON, and wasm are not supported.
Import React from
`react` (or `react/jsx-runtime`) and host APIs from `@orgasmic/plugin-sdk`.
Do not bundle React. The SDK is built with the host and exposes the node
client, transport, hooks, and existing Button/Card/Input/Textarea primitives.
It also exports `uploadAttachment`, `mediaTime`, and the attachment/link types.
Open the app from the selected backend's
origin; a remote-backend profile cannot load another origin's plugin UI.

Export `register(ctx)`, optionally returning a disposer. Register a component
with `ctx.registerNodeView('meetings', View)`. It receives `projectId`,
`collection`, optional `nodeId` (detail, otherwise list), and `onOpenNode(id)`.
Only the plugin's owned collection may be registered. Without a custom view,
the host uses the generic collection/list editor.

Use `ctx.registerStyles(css)` for host-scoped CSS, `ctx.get(path)` and
`ctx.post(path, body)` for project-scoped API requests. `ctx.signal` aborts on
reload, disable, or leaving the project. UI API calls use the signed-in user's
authority, not the plugin's capabilities (`ui.execute` is the trust approval).
Other fetches and module side effects are not managed. Use `ctx.getDraft(nodeId)`, `setDraft(nodeId, value)`, and
`clearDraft(nodeId)` for unsaved edits that must survive view replacement.
Keep the original `base_version` in the draft; never silently retry conflicts
against a fresh version.

Register views and styles only inside `register(ctx)` (until its promise settles
if async); late registrations throw. The daemon stats the installed UI tree
every second. Clients refresh statuses on board events and WebSocket reconnect,
not on a timer. The host imports each new
revision under `/plugins/<id>/ui/<project>/@<revision>/index.js`, so relative
imports reload too. Keep module import free of side effects. Registration is
staged: a failed revision leaves the previous view/styles alive, and one
plugin's failure or throwing disposer cannot block another. This rollback
covers SDK registrations, not arbitrary JavaScript side effects. The host
does not reattempt a failed revision until files change again.

See `examples/plugins/meetings` for a plain-ESM list/detail view and a scoped
notes importer and recording player. Test register, edit/reload with an unsaved draft, save, disable
to the read-only generic fallback, and re-enable before handing a plugin over.

## Links and attachments

Declare the required core services and approve their capabilities. All routes
use the same node ownership/schema gates and caller authorization. Writes to
another plugin's collection are refused. Disabled/unavailable nodes retain
readable metadata, backlinks, and recordings.
Service access also requires permission to read the owning node (`nodes.read`
for command principals); an artifacts-only member cannot read meeting recordings.

- `GET /links?node=ID&incoming=true` returns backlinks. Omit `incoming` for
  outgoing links; `include_deleted=true` includes outgoing tombstones for a
  deliberate restore. `POST /links` takes `source`, `target`, `kind`
  (`RELATES_TO` or `PRODUCES`), `anchors`, `base_revision` (0 on create), and
  `request_id`. Set `deleted:true` to retain a tombstone. There is one record per
  directed source/target pair. A conflicting revision is 409; reload and ask
  before replacing another edit. Keep the request id unchanged when retrying
  the identical request.
- A media anchor is `{attachment, revision, start_ms, end_ms?, label?}`.
  Revision is the immutable payload SHA-256, not the plugin's UI revision.
  Milliseconds are nonnegative safe integers; an end must follow the start.
  The attachment must belong to the source node and be audio/video. Up to 256
  anchors per link. Do not invent duration; the player checks known duration.
  Core stores Org link records beside the source node and indexes backlinks
  on targets. It never copies the recording into a task.
- `GET /attachments?node=ID` lists immutable attachment metadata.
  `uploadAttachment(ctx, node, file, uploadId, onProgress, isPaused?)` uploads
  sequential 4 MiB checksummed chunks, then finalizes. Persist the UUID resume
  handle and reselect the same file after a reload; retry reads the confirmed
  server offset. Use `ctx.delete('/attachments/uploads/ID')` to cancel an
  unfinished upload. Completed attachments are retained, not deleted by this
  API. `ctx.putBytes(path, blob, checksum?)` is the bounded binary transport.
- The protocol is POST `/attachments/uploads` with `node`, `name`, `size`,
  `media_type`, `request_id` (UUID); GET the returned upload id; PUT chunks at
  `/attachments/uploads/ID?offset=N`; POST `/attachments/uploads/ID/finish`
  with optional `sha256`. Every raw request also needs `project` (ctx adds it).
  Uploads are principal-bound, locked, and resumable across daemon restarts.
- `ctx.mediaUrl(node, attachment, revision)` returns a plain content URL for
  native `<audio>`/`<video>`. Media requests use the same-origin cookie session
  established before plugin activation; no token or separate media expiry is
  added to the URL. Normal session expiry and member revocation still apply.
  Plugin command principals use bearer-authenticated content requests.
  Content supports single byte ranges and HEAD, streams in bounded chunks,
  and always uses `nosniff` and `Content-Disposition: attachment`.

## Chat

Conversations are core nodes (`CONV-`) linked to the node they are about; a
plugin never runs an agent. Declare `core.chat@1` (`OPTIONAL` keeps the plugin
usable on an older host) and add `:CHAT_PROMPT: prompts/meeting-chat.org` to
the root drawer: a prompt spec inside the plugin folder (relative path, checked
at install) with the same sections and slots as the shipped `node-chat` spec
(`node.id`, `node.type`, `node.content`, `node.comments`, `node.links`,
`conversation.purpose`). A missing or invalid file falls back to the core
prompt with a warning.

In UI code, `ctx.openChat({ node, purpose, context })` opens the node's newest
open conversation in the dock (or a scoped setup for the first message) and
`ctx.chatContext(chips)` sets or clears the optional chips on whatever the
Chat tab currently shows (`null` clears). Both are absent on a host without
the service: feature-detect `typeof ctx.openChat === 'function'` and hide the
control unless the user has `chat.write`. Chip shapes, all ids rather than
content: `{kind:'node', id}`, `{kind:'attachment', node, id, revision}`
(revision is the attachment's SHA-256 string), `{kind:'range', node,
attachment, revision, start_ms, end_ms}` (end after start), and
`{kind:'selection', text}` (at most 4096 bytes). A range chip on the
conversation's own scoped node becomes a media anchor on the conversation's
link, so the node's Backlinks show the discussed moment. Every send goes as
the signed-in user and needs `chat.write` and `chat.execute`; plugin
principals get `chat.read`/`chat.write` only for conversations about their own
collection, never `chat.execute`.

Limits: 8 GiB per file, 64 GiB per project's local asset store including
reserved uploads, 4096 unfinished uploads (completed receipts do not count). Accepted
types: WAV/MP3/Ogg/MP4/WebM audio, Ogg/MP4/WebM video, PNG/JPEG, PDF, plain text.
Media signatures are checked; HTML, JS, SVG and arbitrary MIME types are not
accepted. Browser codec support still applies. Native media playback is not a
transcoder.

Only `attachments.org` metadata and journals enter the ledger. Payloads and
upload state live under `~/.orgasmic/assets/<sha256-project-id>/`, outside Git.
Moving the project folder preserves access. Pre-merge Slice B builds used a
folder-path hash; if you kept recordings from one, move that project's old
asset directory to the project-ID key before using this build (never overwrite
an existing destination). The ledger metadata and payload hashes stay unchanged.
Back up that store separately; cloning the ledger does not transfer recordings.
Restore assets on another machine before expecting playback. Automatic asset
sync, garbage collection, transcoding, and arbitrary anchor schemas are not
implemented.
