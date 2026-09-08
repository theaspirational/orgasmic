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
collections and commands. UI asset serving is available; automatic view
activation and hot reload are not yet wired. Sidecars, attachments, and chat
remain unavailable.

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
only `core.nodes@1`, `nodes.read`, and `nodes.write` are supported.

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

## UI serving foundation

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
It does not export future P5 services. Scope styles under `[data-plugin=<id>]`.
Keep module import free of side effects and return cleanup from `register(ctx)`;
the automatic activation/runtime contract is the next slice, not implemented
by this serving foundation.
