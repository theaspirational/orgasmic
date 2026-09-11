# Conversations as nodes: scoped, resumable chat scope

Status: scope only. No code, no ledger mutation. Prepared 2026-09-09 against `main` at `d6ab4672` (P0 to P3 and Slice A merged) with Slice B `e3a7e632` on `plugins/links-attachments-player` pending merge. Decision 1 in section 9 was closed by the user on 2026-09-09. The rest are defaults; override any of them before C1 starts.

Companions: `PLUGINS-SCOPE.md` (this document is its `core.chat` service, P5, expanded), `MEETINGS-IMPLEMENTATION-SPEC.md` section 7 (meeting chat; its constraints are folded in here).

## 0. Short version

**Goal.** Chat about anything in the ledger, and come back to it later. A glossary term, a decision, an artifact that can be regenerated, a task with its implementer and reviewer, a meeting with its recording. One model, one verb, one screen. Plugins get it for free.

**Where we are.** There is no conversation anywhere in the repo. A chat is a run. The run id is the only identity. The transcript is the run's session file in `.orgasmic/tmp/`. The supervisor lease key is overloaded as the scope. "New chat" releases the run and the history is gone. Only the Claude harness records a resume command, and only crash recovery uses it. No node view has a "chat about this" entry point.

**What to build.** A conversation is a node in a core collection. Its scope is a link to the node it is about. Its transcript is its runs. One route continues it: live run, else native resume, else cold start with the transcript tail. Two slices: C1 makes the core real and moves project chat and regenerate onto it. C2 moves dispatch onto it and opens it to plugins.

## 1. Where Orgasmic is today

Facts from the code, 2026-09-09. Line numbers drift; function names do not.

- **Project chat.** `POST /api/manager/chat/launch` (`post_manager_chat_launch`, `api.rs`) takes `{project_id, provider, model?, effort?, access?, service_tier?}`. No prompt, no node, no context. The first message is a second call, `POST /runs/:id/input`. The run is acquired with `task_id = "manager.launch:<project>"`, the project's manager singleton, which is why a second chat shows the occupied panel.
- **Persistence.** The conversation is the run's session JSONL under `<project>/.orgasmic/tmp/sessions/` (`orgasmic_core::paths::project_sessions_dir`). Lines are `SessionEnvelope`. Live runs are in memory in the supervisor; the archive is `run_history.rs` under `.orgasmic/tmp/run-history-archive`. Nothing about a chat is in the tracked ledger.
- **Resume.** None. The ACP adapter only calls `session/new` (`crates/orgasmic-drivers/src/adapters/acp.rs`). `NativeRuntimeMeta` (`crates/orgasmic-drivers/src/trait.rs`) has `session_id` and `resume_argv`; only the Claude adapter fills `resume_argv` (`claude --resume <id> --fork-session`). Crash recovery uses it through `POST /runs/:id/recover` with `resume_native_fork`, which mints a new run id linked by `Lifecycle::RecoveryOrigin`.
- **Regenerate is the seed.** `regenerate_node` (`api.rs`) keys the artifactor run on `task_id = "node.regenerate:<node>"`. If that run is live it sends the next round as input; else it cold-launches with `idle_timeout_secs` (default 900, `supervisor.rs`). Prompt slots come from the descriptor's `regenerate_prompt` through `prompt_compiler`. This is the only multi-turn, node-scoped agent session that exists.
- **Dispatch.** `post_task_dispatch` acquires the worker with `task_id = <task id>` and a worktree. Implementer and reviewer transcripts are the same per-run JSONL. Continuing a live worker is `POST /runs/:id/input`. After release there is nothing but recovery.
- **Transcript surfaces.** `GET /runs/:id` returns the whole session text. `GET /ws/transcript/:run_id` (`transcript_stream.rs`) streams snapshot and appends. `GET /runs/:id/native-transcript` finds the harness's own file. The UI folds the stream in `useTranscriptStream` and renders it in `ManagerChatTranscript`.
- **UI.** `RunDockProvider` keeps open tabs in localStorage. The chat tab is a sentinel, `CHAT_TAB_ID`, and the run behind it is found every render by sniffing the harness name (`isNativeChatRun`). `RunSurface` renders any run. The only node-scoped agent entry points are the regenerate and generate dialogs, which never open the dock.
- **Scope today** is the overloaded `task_id`: `manager.launch:<project>`, `manager.launch:<project>#terminal-<uuid>`, `node.regenerate:<node>`, or a task id. `RunSummary` carries `project_id`, `task_id`, `worktree` and nothing else about what the run is about.

## 2. Target shape

**A conversation is a node.** Core collection `conversations`, prefix `CONV-`, a shipped descriptor like decisions and glossary, with write hooks in `node_types.rs`. It gets list and detail views, the generic editor, journals, custom states, CAS, backlinks, git sync, and plugin visibility without new code.

**Scope is a link.** The conversation links to the node it is about through `core.links`, kind `RELATES_TO`, conversation as source. No link means project scope. "Conversations about this node" is the node's existing Backlinks section. Media moments are link anchors.

**Transcript is the runs.** The node lists its run ids, oldest first. Turns never enter the ledger. The session JSONL, the stream, and the chat surface stay exactly as they are. A conversation renders as its runs in order, one segment each.

**One verb: continue.** `POST /conversations/:id/input` with a message and optional context chips. The daemon picks live, resumed, or cold, in that order, and says which. The supervisor lease key is the conversation id, so several conversations in one project can be live at once.

**Context has two parts.** Fixed scope context comes from the node type descriptor, a `chat_prompt` slot next to `regenerate_prompt`. Per-message chips are snapshotted into the input envelope and never rewritten.

**Permissions are explicit.** Reading, writing, and executing are three grants. Executing is never part of a role.

**Nothing new below the node.** No new store, no new event topic, no new transport, no new chat UI. The only new pieces are the descriptor, two routes, the continue policy, and one button.

**Both run modes count.** A conversation's runs may be chat mode (stdio ACP, rendered by `RunChatStack`) or tmux mode (PTY, rendered by `RunTmuxStack`). Both are listed, linked, owned, and continued through the same routes. Chat mode gets structured turns, chips in the message, and native resume. tmux mode gets the typescript as transcript, chips pasted as text, pane reattach while the pane lives, and cold otherwise. Each run records its mode; the UI labels segments. Dock terminals, `manager.launch:<project>#terminal-<uuid>`, are shells, not agents, and are not conversations.

## 3. Record sketch

`.orgasmic/conversations/CONV-7K2Q1/node.org`:

```org
#+todo: OPEN | ARCHIVED
* OPEN Chat about MEET-12 planning
:PROPERTIES:
:ID: CONV-7K2Q1
:PURPOSE: meeting
:OWNER: ["member","anna"]
:PROVIDER: claude
:MODEL: claude-fable-5-1
:EFFORT: high
:ACCESS: workspace-write
:MACHINE: mbp-2024
:WORKTREE:
:RUNS: run-01J9X2 run-01J9Y7
:CREATED_AT: 2026-09-09T10:12:00Z
:END:
Optional user note. Never turns.
```

- `PURPOSE` is an open string. Core knows `discuss`, `regenerate`, `implement`, `review`. Plugins add their own, for example `meeting`. Unknown purposes behave like `discuss`.
- `OWNER` uses the actor key already used by `node_services.rs`: `admin`, `["member",name]`, `["plugin",id,caller]`.
- `MACHINE` is the machine id already used for `.orgasmic/machines/<id>/`. Another machine sees the node and reads it; it cannot replay a transcript it does not have.
- `WORKTREE` is set for `implement` and `review`, empty otherwise.
- `RUNS` is space separated, newest last. The journal records each run start and release with actor and reason, and each cold continuation.
- States `OPEN` and `ARCHIVED` through the per-file `#+todo:` header from P0. Archive is the generic `set_state` edit.
- Link: `links.org` beside the node with one `RELATES_TO` record to the scoped node. Project-scoped conversations have no link.

Write hooks: `PURPOSE` and `OWNER` are required and immutable; `RUNS` and `MACHINE` are daemon-owned and refuse user edits; the generic editor may change the title, the body, and the state.

## 4. Continue

`POST /conversations` creates the node and its scope link, launches the first run, and returns `{id, run_id}`. Body: `{project, purpose, node?, provider, model?, effort?, access?, service_tier?, title?, message?, request_id}`. A `message` sends the first input in the same call. `worktree` is not a client field; purposes `implement` and `review` choose it in C2.

`POST /conversations/:id/input` with `{message, context?, request_id}` returns `{run_id, mode}` where `mode` is one of:

| Mode | Condition | What happens |
| --- | --- | --- |
| `live` | The supervisor holds a run with `task_id == CONV id`. | `send_input`, as today. |
| `resumed` | The newest run in `RUNS` was on this machine and its `NativeRuntimeMeta.resume_argv` is non-empty, or the ACP session id is known and the agent advertised `loadSession`. | New run through the existing `resume_native_fork` path, `Lifecycle::RecoveryOrigin` pointing at the old run. Scope context is resupplied. Run id appended. Journal entry. |
| `cold` | Anything else, including another machine or a deleted session file. | New run. The input is prefixed by the scope context and a bounded tail of the previous transcript when the file exists on this machine, at most 32 KiB, newest turns kept. Journal entry "continued without native memory". The UI labels the segment. |

Rules:

- Scope context is rendered on every new run, never on a live input.
- Chips are rendered into the message inside a fixed delimited block, and stored raw in the session envelope as `context`. Chips carry ids, revisions, and ranges, not content. Selection text is the one exception, capped at 4 KiB. The agent reads more through the `orgasmic` CLI and its tools.
- Idle release uses `idle_timeout_secs`, default 900 like the artifactor. Purposes may override. `implement` and `review` have none.
- Stop is the existing `POST /runs/:id/release`. It does not archive the conversation.
- The ACP adapter starts recording its session id into `NativeRuntimeMeta.session_id` and learns `session/load`. Claude's `resume_argv` path works today and is the first resumed mode to ship.
- Continue is refused with 409 while a resume launch is in flight for the same conversation. The per-node write lock from P0 serializes `RUNS` edits.

Everything else is generic. List: `GET /graph/nodes?layer=conversations`. About a node: `GET /links?node=X&incoming=true`, sources with the `CONV-` prefix. Read: `GET /org/node?id=CONV-…`. Transcript: `GET /ws/transcript/:run_id` per run in `RUNS`. Archive: the generic `set_state` edit.

## 5. Context

**Descriptor slot.** `NodeTypeDescriptor.chat_prompt` next to `regenerate_prompt` (`crates/orgasmic-core/src/node_type.rs`). It names a prompt spec compiled by `prompt_compiler` with slots `node.id`, `node.type`, `node.content`, `node.comments`, `node.links` (linked ids and titles, both directions), `conversation.purpose`, `conversation.instructions`. Core descriptors ship one each. Tasks reuse the dispatch bundle slots (`task.id`, `dispatch.brief`, handoff). Artifacts reuse `assemble_artifact_context` for subject nodes. Plugins set `:CHAT_PROMPT:` in `plugin.org` to a prompt file inside the plugin folder. A collection without a slot gets the core default: id, type, content, comments, links.

**Chips.** The composer shows the chips that will go with the message: the scoped node always, and optionally a node under the cursor, an attachment with its exact revision, a time range, a selection. The user removes optional chips before sending. The sent message shows the chips it carried. A later selection change never edits a sent message.

**Bounded reads.** No transcript, recording, or full subject tree is pasted into a prompt by default. The agent runs on the host with the `orgasmic` CLI and reads what it needs. Plugin commands run through `plugin run` with the plugin principal, as today.

## 6. Trust and permissions

Say what a chat can reach. Never imply a boundary that is not there.

- **Chat executes on the host.** A conversation run is an agent process with host filesystem authority and, for `access` above read-only, shell. Prompt scoping does not confine it. The meetings spec says so; this document repeats it.
- **Three actions.** `chat.read` reads conversation nodes and their transcripts in a project. `chat.write` creates conversations and continues one's own; owner or admin only, so a member never silently continues another member's privileged session. `chat.execute` is the trusted-host-agent grant. Creating or continuing a conversation requires both `chat.write` and `chat.execute`.
- **Roles.** Viewer gets `chat.read`. Editor gets `chat.read` and `chat.write`. No role contains `chat.execute`. An admin grants it per member, explicitly, through the existing member grant path, and the enable text says the agent runs on the host as the daemon's OS user.
- **Node authority still applies.** Reading a conversation about node X requires read on X, the same `node()` rule as links and attachments. A member who cannot see a meeting does not get its recording through the conversation's chips or anchors.
- **Plugins.** Capabilities `chat.read` and `chat.write` for plugin principals, scoped to conversations about the plugin's own collection. Plugin commands never launch or continue runs in v1; no `chat.execute` for plugins. Plugin UI acts as the signed-in user, as decided in P3.
- **Transcripts are sensitive.** They may contain file contents and secrets the agent read. `chat.read` is project-wide on purpose, like `graph.read`; do not put transcripts on routes that skip it. Media grant URLs, tokens, and session paths are never rendered into chips or journals.
- **Events.** Turns stream on `Topic::Run` as today. Node changes emit the existing graph events with layer `conversations`. No new topic.

## 7. Delivery plan

Two slices. C1 is self-contained and replaces two existing chats. C2 opens it up.

### C1, conversation core

- Descriptor, write hooks, `#+todo:` states, journal entries.
- `POST /conversations` and `POST /conversations/:id/input` with the three-mode policy. Lease key is the conversation id.
- Resumed mode through `resume_native_fork` for Claude. ACP adapter records session ids and calls `session/load` when advertised. Cold mode with the bounded tail.
- `chat_prompt` slot and core defaults. Chips: scoped node only.
- Actions `chat.read`, `chat.write`, `chat.execute`; roles; member grant; plugin capability mapping in `authz::require` and `plugins.rs`.
- Project chat moves onto it: `post_manager_chat_launch` becomes a shim that creates a `discuss` conversation with no link. The `manager.launch:<project>` chat lease goes away; terminals keep theirs. The occupied panel goes away.
- Regenerate moves onto it: purpose `regenerate`, one conversation per artifact, `commit_artifactor_regenerate_round` stays as the purpose's post-turn hook. `NodeRegenerateControl` opens the conversation in the dock so the user can talk to the artifactor.
- UI: the dock's chat tab lists the project's conversations, open first, live marked, plus New chat. A conversation opens `RunSurface` on its current run with older runs as collapsed segments. Cold segments are labeled. A Chat button on `GenericNodeView`, `GenericNodeDialog`, `TaskDialog`, `ArtifactView`, and `NodeDocEditor`. Backlinks groups `CONV-` sources under "Conversations" with purpose and state.

Exit: create and continue live; release then continue gives resumed on Claude and cold elsewhere; idle release then continue gives resumed; two live conversations in one project; a viewer reads but gets 403 on create and continue; a member without `chat.execute` gets 403; a member cannot continue another's conversation; a plugin principal reads conversations about its collection and nothing else; project chat and regenerate regression tests pass unchanged in behavior; the daemon route tests spin a real daemon as in P2 and Slice B.

### C2, purposes and plugins

- Dispatch creates a conversation per attempt kind: purpose `implement` or `review`, linked to the task, `WORKTREE` set, `RUNS` appended per attempt. The worker lease keeps `task_id = <task id>` so handoff, tx rows, and verdict code are untouched; the conversation records the runs. Continue on a live worker sends input; on a released worker it resumes on Claude or refuses with "dispatch a new attempt" where no resume exists. Say so in the UI.
- Plugin `core.chat@1`: `ctx.openChat({node, purpose, context})` opens or creates the node's conversation in the dock; `ctx.chatContext(chips)` sets optional chips for the composer; `:CHAT_PROMPT:` in the manifest.
- Anchor rule in `post_link` relaxes from "attachment owned by the source" to "owned by the source or the target", same audio/video and exact-revision checks, so a conversation can anchor moments in the meeting it is about.
- Meetings chat: purpose `meeting`, chips for attachment, revision, time range, selection; the plugin appends a link anchor per discussed moment.
- Then the agent-callable media commands, `plugin run meetings <command>`, from the meetings spec M5. That is media work, not chat work, and lands after C2.

Exit: dispatch an implementer, see its conversation on the task, send it a message while live, resume it after release on Claude; the meetings player opens a chat with a time-range chip and the task backlink shows the anchored moment.

## 8. Existing chats mapped

| Today | Conversation |
| --- | --- |
| Project chat, `manager.launch:<project>` | No link, purpose `discuss`. Many per project. |
| Artifact regenerate, `node.regenerate:<id>` | Link to the artifact, purpose `regenerate`, post-turn hook writes the version. |
| Task implementer dispatch | Link to the task, purpose `implement`, worktree, per-attempt runs. C2. |
| Task reviewer dispatch | Second conversation on the same task, purpose `review`. C2. |
| Glossary, decision, any node | Link to the node, purpose `discuss`. |
| Meeting | Link with media anchors, purpose `meeting`, plugin context. C2. |
| Terminals, `manager.launch:<project>#terminal-…` | Unchanged. Not conversations. |

## 9. Decisions

| # | Decision | Chosen |
| --- | --- | --- |
| 1 | Where a conversation lives | A node in the ledger, core collection `conversations`. Closed by the user 2026-09-09. |
| 2 | Scope representation | A `core.links` record, kind `RELATES_TO`, no new edge kind. Default. |
| 3 | Turns in the ledger | Never. Runs hold them. Default. |
| 4 | Cold continuation | Allowed, labeled, journaled, bounded to 32 KiB of tail. Default. |
| 5 | Execution grant | `chat.execute` per member, never in a role. Default. |
| 6 | Concurrency | Many live conversations per project; supervisor caps apply. Default. |
| 7 | Dispatch migration | Dual-keyed in C2: worker lease stays on the task id, the conversation records runs. Default. |
| 8 | Resume order | Claude `resume_argv` first, ACP `session/load` second, cold last. Default. |
| 9 | Ownership | Owner or admin continues; everyone with `chat.read` reads. Default. |
| 10 | Run modes | Both chat mode and tmux mode are conversations. tmux gets typescript transcript, pasted chips, reattach only. Dock terminals are excluded. Default. |

## 10. Not in scope

- Syncing transcripts between machines. The node says which machine has them.
- A new chat UI, backend, transport, or LLM integration. `RunSurface` and the stream are it.
- Confining host execution by prompt. The grant is the boundary.
- Summaries, memory, or embeddings beyond the transcript tail.
- Multiple people typing into one conversation. Owner-only continue.
- Editing or deleting sent turns.
- Sidecars. Chat needs none.
- Voice, screen capture, or transcription inside chat. Those are meetings media commands.
