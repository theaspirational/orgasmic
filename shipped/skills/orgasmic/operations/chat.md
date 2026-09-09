---
type: Operation
title: Conversation and chat operations
description: Grant chat access, read a conversation's runs and journal, and know
  how a conversation is continued, resumed, and archived.
aliases:
- orgasmic member add --action
- orgasmic member set-actions
- chat.read
- chat.write
- chat.execute
- conversation
sources:
- cli-help/member.txt
- crates/orgasmic-daemon/src/conversations.rs
- crates/orgasmic-daemon/src/authz.rs
- shipped/schema/node-types/conversation.org
tags:
- chat
- authorization
---

# Conversation and chat operations

A conversation is a node in the `conversations` collection (`CONV-…`), scoped
to one node through a `RELATES_TO` link in its `links.org`, or to the project
when it has no link. Its transcript is its runs: turns never enter the ledger.

## Grants

| Action | What it allows | Who has it |
| --- | --- | --- |
| `chat.read` | Read conversations and their transcripts in a project | viewer and above |
| `chat.write` | Create a conversation and continue one's own | editor and above |
| `chat.execute` | Start the agent process itself | admin only, plus members granted it explicitly |

Creating or continuing a conversation needs `chat.write` **and**
`chat.execute`. Reading a conversation about a node also needs read on that
node.

`chat.execute` is not in any role, and it is not a formality: it lets the
member run an agent on this host, as the OS user the daemon runs as, with that
user's files and credentials. Grant it only to people you would give a shell.

```bash
orgasmic member add anna --role editor --action chat.execute
orgasmic member set-actions anna chat.execute   # replace the whole set
orgasmic member set-actions anna                # revoke every explicit action
```

Explicit actions apply on every project the member holds a role for and take
effect on their next request.

## Purposes

`discuss` is an ordinary chat. `regenerate` is the artifactor's conversation
about one artifact, reused for every round. `implement` and `review` are the
conversations a dispatch opens on its worker attempts; they are created by
dispatch, never by hand, and a plugin may name its own purpose (the meetings
example uses `meeting`).

## Runs

`:RUNS:` lists the conversation's runs oldest first, and the last entry is the
current one. A suffix says how that run began:

| Entry | Meaning |
| --- | --- |
| `run-…` | the first run, or a dispatch attempt |
| `run-…:resumed` | continued into the agent's own session |
| `run-…:cold` | continued without native memory, from a scope prompt and a bounded transcript tail |

Continuing a conversation whose run has ended tries, in order: Claude's own
fork of the native session, an ACP `session/load` when the agent advertises
it, and only then a cold start. An `implement` or `review` conversation never
goes cold: without its worker's session there is nothing to continue, so the
route answers 409 `no_resume` and the next attempt is a new dispatch.

`SERVICE_TIER` and `HARNESS_ARGS` are the launch inputs each relaunch repeats.
They, along with `RUNS`, `MACHINE`, `MODE`, `PROVIDER` and `CREATED_AT`, are
daemon-owned: an edit through the node editor is refused. `PURPOSE` and
`OWNER` are immutable.

## Journal

A conversation's `journal.org` records `conversation.run_started`,
`conversation.run_resumed`, `conversation.run_cold` and
`conversation.run_released`. A release entry names the reason the run ended
(`idle_timeout_exceeded` for the idle sweep, the operator's own reason for a
stop) and the actor who asked for it, or the daemon when nothing did. Session
file paths never appear in it.

## Stopping and archiving

Stopping a run is `POST /runs/:id/release`; it ends the run and leaves the
conversation open, so the next message continues it. A member may release only
the conversation runs they own. Archiving is an ordinary state edit to
`ARCHIVED`; an archived conversation refuses new input and stays readable.

## Plugins

A plugin declares `core.chat@1` and may hold `chat.read` and `chat.write`
scoped to conversations about its own collection. No plugin is ever granted
`chat.execute`: every send runs as the signed-in user and is gated by that
user's grants.
