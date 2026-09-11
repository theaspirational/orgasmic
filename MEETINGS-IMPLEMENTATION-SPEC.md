# Meetings: native Orgasmic workspace

Status: implementation handoff; no feature implementation or ledger mutation accompanies this document.

Prepared: 2026-09-07. Source baseline: `d12baf6dea8046c634e6dd42ba1c3495f1266f36`.

## 1. Intent and authority

Make a meeting a first-class Orgasmic node and a native collaborative workspace. Users bring recordings, transcripts, and supporting files into a meeting; review them with teammates and Orgasmic's agent; and create, correct, discuss, and trace ordinary Orgasmic nodes back to the conversation.

The reference experience is the existing ORSL/Max review interface: linked work on the left, recording and transcript in the middle, editable selected work on the right. Integrate that interaction into Orgasmic, with its native chat, identities, node editors, journals, and lifecycle rules. Do not embed the standalone application.

**The workspace is about all node types, not just tasks.** Tasks, decisions, glossary items, artifacts, and other Orgasmic nodes can be linked to a meeting or be products of it. An artifact can be an output of a meeting; a meeting is not an artifact subtype.

Section 2 captures the user-confirmed product direction; sections 3–4 make it concrete with proposed workflow and interaction defaults. Later sections prescribe an implementation approach grounded in the inspected source. Revalidate interfaces against the current checkout before coding; adjust concrete filenames and endpoint spelling to current repository conventions without weakening the product requirements.

Read `AGENTS.md` and the live project conventions first. In this environment, durable design authority lives in `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/`, not a `.orgasmic/` directory in this checkout. This file is a handoff specification, not a new ADR system or a substitute for that authority. Any subsequently requested ledger updates must go through the supported CLI/daemon.

## 2. Required outcome

| ID | Requirement |
| --- | --- |
| R1 | A real `Meeting` node type and a dedicated project-level Meetings page. One entry represents one meeting, not one uploaded file. |
| R2 | Upload video, audio, transcripts, subtitles, images, and supporting documents as managed node attachments. Retain originals. A meeting works with any subset, including text only. |
| R3 | A meeting workspace with media playback, editable transcript/subtitles, linked nodes, native node editing, screenshots/annotations, and team comments. |
| R4 | Use Orgasmic's native agent chat inside the meeting. The agent can perform media work and create/link actual Orgasmic nodes, not merely suggest commands in prose. |
| R5 | Both people and agents can create nodes from the meeting, link existing nodes, and assign or correct recording positions/ranges. |
| R6 | Support every resolvable Orgasmic node type. Clearly distinguish a node related to the meeting from a node produced by it. |
| R7 | Multiple fragments, recordings, and meetings can refer to the same node. Clicking a fragment seeks to its actual source. The node's ordinary view provides meeting backlinks. |
| R8 | Team members with appropriate permissions can edit nodes, create nodes, comment, and attach annotated screenshots in the same workspace. Changes use normal Orgasmic persistence and attribution. |
| R9 | Editable sidecar subtitles, transcript corrections, synchronization, and annotations survive reloads and do not destroy source material or silently invalidate citations. |
| R10 | Work on desktop and a remotely connected tablet, with authenticated uploads, playback, comments, editing, and recoverable interruptions. |

There is no mandatory proposal/acceptance stage between extraction and ordinary nodes. A request to create tasks creates real tasks in a valid normal lifecycle state; it does not start implementing those tasks. Existing lifecycle, validation, and safety rules still apply. Uncertain interpretations must be identified, not presented as meeting agreements.

## 3. Primary workflows

### 3.1 Bring in a meeting

1. Open Meetings and choose New meeting.
2. Enter a title; optionally date/time, timezone, participants, and notes. Participants need not be Orgasmic members.
3. Drop recordings, transcript/subtitle files, notes, and screenshots onto the meeting. Show upload and processing states independently for each file/job.
4. Play any available recording immediately after it is ready. Pick a preferred playback source when several exist. Text-only meetings expose transcript/document review without a broken player.
5. Open the native chat and ask, for example: “Synchronize the Plaud audio with the screen recording, create Ukrainian subtitles, and extract the tasks and decisions with timecodes.” Show real operations, progress, resulting attachments, and resulting nodes.

### 3.2 Review and correct work

1. Select a linked task, decision, term, artifact, or other node in the left list.
2. Edit its actual title/body/type-specific fields in the right pane using the native editor. Display the node's real lifecycle state, not a meeting-specific replacement status.
3. Click any source fragment to select the right recording, seek, and highlight the relevant transcript passage. Selecting a node alone must not unexpectedly start playback.
4. Add another fragment using the current time, a selected range, a transcript selection, or an explicitly entered timecode.
5. Capture the current video frame or upload an image, annotate it, and attach it to the selected node with meeting/source provenance.
6. Discuss the node in its normal comment history. Add meeting-wide or fragment-specific comments without confusing them with node comments or agent chat.

### 3.3 Create or link manually

At any playback position or transcript selection, choose Create node or Link existing. Create opens the native type picker and creation form with the source prefilled. A new node created through this action is a meeting product; linking an existing node defaults to Related. Both paths allow no timecode when the source is the meeting generally or an untimed document.

For example, `00:26:35` can support a task about Pending Edits, a decision about comparison behavior, and a glossary entry explaining a term. Those remain three ordinary nodes, each independently editable and independently linked to other meetings.

### 3.4 Return from elsewhere

A task on the normal board, a glossary entry, a decision, or an artifact shows its meeting sources. Opening a source navigates to the meeting, selects the node and recording, and seeks to the fragment. The same link can be shared with an authorized teammate. Changing the node in its ordinary view updates the meeting view; unlinking it from the meeting never deletes it.

## 4. Workspace and interaction design

Use the existing Orgasmic shell, typography, tokens, primitives, and light/dark themes. This is a dense working surface, not a new dashboard visual language.

```text
Orgasmic project navigation: … Tasks | Decisions | Glossary | Artifacts | Meetings

Meeting title · date · participants          Add files · source selector · menu
┌──────────────────┬─────────────────────────────┬──────────────────────────┐
│ Linked nodes     │ Recording / audio player    │ Selected native node     │
│ Search + filters │ transport + subtitle track  │ type · title · lifecycle │
│ All / Produced / │                             │ source fragments         │
│ Related          │ Capture frame · Create here │ body / native fields     │
│                  ├─────────────────────────────┤ attachments / annotation │
│ New · Link       │ Transcript / Files /        │ native comments/activity │
│ type + title     │ Meeting discussion          │                          │
└──────────────────┴─────────────────────────────┴──────────────────────────┘
Native Orgasmic chat dock: meeting-scoped conversation · context chips · runs
```

The sketch defines responsibilities, not fixed pixel widths. Reuse the existing dock rather than squeezing an always-visible fourth narrow column into the page.

- **Meetings index:** a compact searchable list showing title, date, source availability, linked/product counts, and active/failed processing when present. New meeting and multi-file upload are the primary actions. Archived meetings are filtered out by default, not erased.
- **Linked nodes:** type label/icon, title, real lifecycle state when applicable, relationship badge, and source count. Type and relationship filters; task filtering remains useful, but the collection is not named Tasks. Preserve selection while background updates arrive.
- **Player:** native video/audio controls, source selector, current position/duration, editable subtitle-track selector, and clear unavailable/processing/unsupported states. A transcript selection can set a range. Range playback is optional; accurate seeking is required.
- **Transcript:** speaker, clickable timecode, editable text, search, current-cue highlight, and original/source-time display when different from playback time. Playback following can be disabled by scrolling; offer Resume following. Do not steal keyboard focus on every cue.
- **Selected node:** existing editor semantics and comments, source chips, add-current-time action, relationship controls, and attachment preview/annotation. An artifact uses its existing artifact view and valid edit/regenerate actions; it must not become a freeform unsafe HTML editor.
- **Empty selection:** show meeting details/notes with actions to create or link a node; do not manufacture a placeholder task.
- **Native chat:** show persistent meeting identity and explicit context chips for selected node/source/range. A user can remove optional focus context before sending. Chat and human meeting discussion are different surfaces with different permissions and histories.
- **Tablet:** when three useful panes do not fit, use switchable Nodes / Recording & transcript / Details, plus the existing chat drawer. Preserve playback and drafts across pane switches. Portrait must not require desktop horizontal scrolling. Essential controls need touch-sized targets; annotations must support touch/stylus without hover.
- **Accessibility:** keyboard-operable node list, source links, dialogs, player actions, and annotation controls; visible focus; labeled buttons; no global playback hotkeys while typing. Represent relationship and job state with text as well as color.
- **Save feedback:** Saving, Saved, Offline/Retry, or Conflict. Never show Saved before the server acknowledges. Preserve unsent text when switching panes, losing connectivity, or encountering a conflict.

Deep links carry meeting ID, optional node ID, attachment ID/revision, and position/range. These are navigation hints, not authorization. Invalid or inaccessible selections produce a recoverable message without revealing private titles. Do not put auth tokens, transcript text, or signed media URLs into shareable links.

## 5. Domain model and invariants

### 5.1 Terms

| Term | Meaning and boundary |
| --- | --- |
| Meeting | A first-class node representing a conversation and its review workspace. Its identity does not depend on a particular recording. |
| Node reference | Project-qualified reference to an existing native node. The server resolves its type and permissions; the client does not infer the type by “otherwise glossary.” |
| Attachment | A managed file belonging to a node, with durable metadata and immutable payload revisions. Not automatically a graph node or an artifact. |
| Meeting link | One authoritative relationship from a meeting to a node, optionally carrying several source anchors. |
| Related | The node was discussed, referenced, or otherwise connected to this meeting. Does not claim the meeting created it. |
| Produced | The node is a product of this meeting. Does not imply exclusive ownership, a parent/child relationship, or that every later edit came from the meeting. |
| Source anchor | A stable reference to a recording position/range, transcript passage, or document excerpt in a particular source revision. |
| Alignment | An explicit mapping between the clocks of two source revisions. It may contain gaps, cuts, or several segments. |
| Processing operation | A recoverable run that derives outputs from specified source revisions. Its lifecycle is not the meeting's lifecycle. |

### 5.2 Meeting node

Ship a descriptor for collection `meetings`, with a proposed `MEET-` ID prefix using the existing minted suffix convention. Include the standard node identity/title fields and bounded meeting metadata: optional occurrence time and timezone, participant labels/member references, notes, and preferred playback attachment. Keep unknown occurrence times unknown; do not require a fabricated timestamp to import files.

Use a simple active/archived meeting state if the descriptor needs states. Transcribing, aligning, failed, and ready belong to operations/attachments, not to a single state machine pretending an entire meeting is one job. A valid meeting can have one ready video and one failed transcript operation.

### 5.3 Node-generic meeting links

Logical contract; this is a proposed shape, not a claim that these types already exist:

```ts
type NodeRef = { projectId: string; nodeId: string };
type AttachmentRef = { attachmentId: string; revisionId: string };

type SourceAnchor =
  | { id: string; kind: 'media'; source: AttachmentRef;
      startMs: number; endMs?: number; label?: string }
  | { id: string; kind: 'transcript'; source: AttachmentRef;
      cueIds: string[]; label?: string }
  | { id: string; kind: 'text'; source: AttachmentRef;
      quote: string; context?: string; label?: string };

type MeetingLink = {
  id: string;
  meeting: NodeRef;
  target: NodeRef;
  relation: 'related' | 'produced';
  anchors: SourceAnchor[];
  revision: string;
  // Server-derived creator/editor attribution and timestamps accompany writes.
};
```

Invariants:

1. One active link per meeting/target pair. Add fragments to it; do not create duplicate sidebar entries. A node can have links from many meetings, including more than one Produced relationship.
2. Link metadata never stores another editable copy of the target's title, description, lifecycle, or comments. Read models may cache these fields as disposable projections.
3. New-node creation initiated within the meeting creates the node and its Produced link as one idempotent, recoverable operation. It must not expose success with a permanently missing half. Linking an existing node defaults to Related; an explicit, attributed correction can change the relationship.
4. Editing an existing node from a meeting does not automatically turn it into a product. Removing a link leaves the node and its unrelated attachments intact.
5. The target may be a task, decision, term, artifact, meeting, or another registered/generic node. Reject a meeting's link to itself. Linking an existing resolvable node does not require that its type supports creation or editing in this UI.
6. V1 supports references within the same project. Keep project qualification in the contract and clearly reject cross-project writes until resolution and authorization are implemented end to end. In particular, do not claim current artifact cross-project IDs already work.
7. Read-only singleton/legacy nodes may be linked if the resolver can address them; show their actual supported actions. Generic types must have a safe descriptor-driven or read-only fallback, never accidentally use the glossary writer.
8. Media anchors use nonnegative integer milliseconds, not formatted strings. A point has only `startMs`; a range has `endMs > startMs`. Validate against known source duration. A meeting-wide link may have no anchors.
9. References name immutable source revisions. Replacing a recording/transcript does not silently retarget older citations to different content. Offer explicit re-anchor/remap, with history and preview.
10. Missing, archived, deleted, or inaccessible targets/sources have distinct outcomes. Retain resolvable provenance where policy permits; do not cascade-delete outputs or disclose denied metadata.

Use the closed graph vocabulary. Project meeting links as `RELATES_TO` or `PRODUCES`, retaining the rich anchors in the link record. Extend relevant class constraints, parsers, validation, and graph projections deliberately; do not invent `SOURCED_FROM` or `MEETING_OUTPUT` edges. There must be one write authority for a meeting link: graph edits and workspace edits, if both supported, call that same command. Reverse backlinks and graph edges are derived, not independently editable copies.

### 5.4 Persistence

Follow the existing per-node directory kernel. Proposed type extras:

```text
<ledger>/.orgasmic/meetings/MEET-XXXXX/
  node.org          # canonical meeting metadata and notes
  journal.org       # attributed activity/comments, existing journal conventions
  links.org         # canonical meeting→node links and anchor records
  attachments.org   # generic node attachment manifest

<any other node directory>/attachments.org
<daemon-managed asset root>/...  # immutable payloads and temporary uploads
```

Store link and attachment metadata in parseable Org headings/properties, with stable record IDs and explicit schema versioning. Attachment IDs are unique within the project, resolve to an owning node, and do not grant access by themselves; node references use durable project identity, not a display name or checkout path. Use nested anchor/revision records rather than an opaque serialized UI state blob. Cue lists, alignment segments, and annotation geometry can be structured JSON attachment payloads; videos and images remain binary payloads. Reserve type-extra filenames in the relevant descriptors and teach index/replay/watch paths about them.

Large files live outside the Git ledger in one daemon-managed local asset store, using the existing home/path conventions. Select the concrete asset-root location during implementation; never persist user-supplied arbitrary filesystem paths as readable download URLs. Metadata stores an opaque storage key/content digest, byte length, detected media type, original display name, revision identity, and source lineage. Original filenames are labels, not paths.

Identical immutable payloads may share storage. Attachment ownership, permissions, and logical identity remain separate; knowing a content digest never grants access. Externally generated files enter through the same attachment commit path as uploads. Backups/export must include the referenced asset closure, not just a ledger clone; a missing blob is reported as missing, not as a successful empty attachment. No automatic destructive garbage collection in this feature.

All structured writes go through daemon commands and the serialized writer. Check expected revisions at the actual serialized mutation boundary, not only before queuing a write. Reuse transaction and request-ID infrastructure; add only the missing multi-record validation/recovery needed for create-and-link and attachment commit. Do not assume `transaction_multi` alone supplies every required compare-and-swap or filesystem crash guarantee.

## 6. Attachments, media, and editable source material

### 6.1 Upload, storage, and playback

- Provide one node-attachment service used by meetings and other nodes. Support multi-file selection, individual progress, cancel, retry, and bounded-memory streaming. For large recordings, persist resumable upload state so a tablet reconnect does not require a complete restart.
- A small sequential chunk protocol is sufficient: create upload, send bounded chunks with expected offset, query confirmed offset, finalize, cancel. Scope the upload session to actor/project/owner node. Validate size/quota, offsets, declared versus detected type, final length, and checksum before publishing a ready attachment. Temporary partial data is not visible as a completed source.
- Probe media after safe ingestion. Keep the original; derive a browser-playable version only when necessary or requested. Show unsupported format/missing tool errors with the next action, without claiming the file was transcribed or repaired.
- Serve authorized `GET`/`HEAD` and byte-range responses, including `206`, valid `Content-Range`, and `416` for unsatisfiable ranges. Seek without downloading the entire recording into a JavaScript Blob. Reuse immutable revision validators/cache policy with authorization-aware responses.
- Extend `ui/src/lib/transport.ts` for binary upload and authenticated asset URL acquisition. Do not scatter ad hoc `fetch` calls or create a second browser-to-daemon transport layer.
- Same-origin web playback can use the existing authenticated cookies. Native/remote profiles may hold bearer credentials that a `<video>` tag cannot attach. Provide transport-mediated, short-lived, attachment-revision-scoped media access or an equivalent existing native bridge; never put the global daemon token into a player URL. Handle expiry by refreshing access and restoring position.
- Media access must remain checked for each new request/range, including after member access is revoked. Scoped media grants must not outlive authorization without a documented, bounded revocation mechanism. Do not log credentials or leak media grant URLs into chat history, exports, referrers, or shared navigation URLs.
- Render uploaded text as text/sanitized content; do not execute uploaded HTML/MDX/scripts. Use safe MIME/disposition behavior and keep untrusted active content off the authenticated application origin. Artifact image blocks should resolve authorized attachments through the same safe asset mechanism.

### 6.2 Transcript and subtitle model

Accept existing transcript text and common sidecars, including SRT and WebVTT. Preserve the original bytes, language, speaker labels, original clock, and importer information where available. An untimed transcript is valid; do not fabricate timing for it.

Use one editable timed-transcript representation: ordered cues with stable IDs, speaker, text, start/end milliseconds when known, and a reference to their clock/source. Keep canonical cue data in a versioned structured attachment. WebVTT and SRT are generated/exportable sidecars for a chosen transcript revision and target playback source, not a second independently mutable truth. Importing an edited subtitle file creates a new explicit revision.

Provide text/speaker/timing edits and cue split/merge. Preserve cue IDs when identity survives; record explicit old→new correspondence for split/merge, or leave old citations on the retained source revision. Reordering or inserting a cue must not shift every later citation by numeric array index. Validate malformed timings and distinguish intentional speaker overlap from invalid ranges.

Video subtitles render as a selectable track. Editing and saving a transcript updates the associated rendered track; exporting produces a real editable `.vtt` or `.srt` file. Burn-in may be an optional requested derivative, never the only subtitle output. Large transcripts should not cause every video `timeupdate` to re-render every node editor.

### 6.3 Synchronization and alignment

Model a mapping between specific source revisions as ordered segments, each with source interval and target interval. Linear mapping within a segment supports offset and drift; absent segments represent unavailable content. Require monotonic, nonambiguous segments for a given mapping. Do not guess across gaps.

When a transcript passage crosses a cut, resolve it into several playable fragments or mark the unavailable portion. When a source is replaced, retain the old mapping and anchors; create a new mapping revision and explicitly apply/review remapping. Show original time and playback time when they differ. Store original transcript anchors and resolve through the selected alignment rather than destructively rewriting every original timecode.

The agent can inspect sources, suggest alignment, generate synchronized playback, and report verification points. Users can correct offsets/segments when automatic alignment is uncertain. Preserve separate original tracks and lineage; producing a mixed recording must not duplicate a voice already present in another source. Mixing parameters and selected inputs are recorded with the operation.

### 6.4 Screenshots and annotations

Capture the actual selected playback frame with its recording revision and position. Browser capture may use an authorized same-origin frame; use a daemon frame-extraction operation where browser security/codec limitations prevent it. Uploading an image is equally supported.

Attach the result to the selected native node, or to the meeting if no node is selected. Preserve the original image and source provenance. Annotation data references that image revision and stores normalized geometry plus original dimensions; support pen, arrow, rectangle, and text, with undo, selection, movement, and deletion. A rendered annotated PNG is a derivative for easy export, not a replacement for editable annotation data. Prefer the existing drawing dependency/native canvas tools over a new editor framework.

## 7. Native agent chat and actual operations

### 7.1 Conversation scope

Extend the existing chat launch/run metadata with explicit meeting scope. Persist a conversation identity, project/meeting reference, owner/actor, and its run/session association in the existing conversation/run persistence. A browser local-storage tab is not the durable association. Reopening a meeting restores or selects its native conversation; continuation after a new run must not lose earlier scope/history.

Use `RunDock`, `RunSurface`, `ManagerChatTranscript`, `ManagerComposer`, existing provider selection, and the current streaming/session lifecycle. No embedded third-party chat and no meeting-specific LLM backend. Keep project chat distinct. Switching meetings must not retarget an already-running conversation.

Each message snapshots its context: meeting ID, optional selected node ID, attachment revision, time/range, selected cue IDs/text, and explicit user instructions. Display these chips before sending. Make meeting notes, source manifests, linked-node summaries, and full source content available through bounded reads/tools; do not paste every transcript into every prompt. A subsequent selection change does not rewrite a message already sent.

For collaboration, default to a member's own meeting conversation rather than accidentally continuing another person's privileged session. Sharing/readback/interacting with a session follows existing session permissions and actor attribution. Human comments remain in journals; do not use the agent transcript as the meeting's team discussion thread.

### 7.2 Capabilities to expose through native tools/CLI

These operations must have real daemon-backed contracts available to both the UI and authorized agents:

| Operation | Required behavior |
| --- | --- |
| Inspect meeting/source | Read meeting, permitted links, attachment metadata, transcript slices, and source/time mappings. |
| Transcribe | Process selected audio/video into a versioned transcript with timestamps/speakers when the configured engine supports them; report limitations. |
| Align/mix | Relate specified source clocks and optionally create synchronized playback while retaining inputs. |
| Generate subtitles | Produce downloadable sidecars for a named transcript revision and playback clock. |
| Create node from meeting | Invoke the appropriate native creation command and persist its Produced relationship/anchors idempotently. |
| Link/update node | Resolve existing nodes; link/re-anchor or edit using normal validation, expected revisions, and permissions. |
| Create artifact | Reuse the artifact generation/submission contract; link the durable artifact node as a product. Running generation is not an already-ready artifact. |
| Capture/attach image | Extract a frame or attach a file with source lineage; use the shared attachment service. |
| Inspect/cancel/retry operation | Report actual run state/output revisions and preserve usable prior outputs on failure. |

Use the installed/configured media toolchain where available (for example FFmpeg for probe, frame extraction, and remux/mix). Ship at least one tested transcription execution path, not only a UI button or an aspirational adapter. Select/configure the engine using existing settings conventions. Do not silently upload recordings to a provider, incur an unconfigured paid service, install binaries, or embed credentials. Missing configuration must say exactly what is needed; manual import/edit/link workflows remain available.

An extraction request can create tasks, decisions, terms, artifacts, or another supported type. Default task creation to the ordinary backlog state, with inferred owners/dates left unset unless supported by the source or user instruction. Persist source evidence and describe uncertainty. Meeting citations do not replace task execution `evidence`, verification, or lifecycle rules. Avoid creating duplicates on retry; search/link existing nodes where the request calls for reconciliation. If the user asks only for analysis, do not turn that into node creation.

When asked to reconcile an existing set with the recording, the agent must read the current native node contents and sources, identify omissions/contradictions, and apply authorized corrections to those same nodes with normal revision checks. Preserve the user's edits and change history. Do not regenerate a parallel list, silently replace current content with the original import, or leave two competing descriptions. The completion response identifies changed/created/linked node IDs and any unresolved source ambiguity.

### 7.3 Operation durability and safety

Reuse the native run/session supervisor for execution, cancellation, progress, and logs. Add bounded media-operation metadata: operation ID, actor, request ID, input revisions, parameters/tool version, state, output refs, and error/recovery information. Do not add a second general workflow engine or job database.

Persist enough state to distinguish queued/running/succeeded/failed/cancelled/interrupted after restart. On recovery, reconcile with the actual process; never leave a dead job reporting Running forever. Retry may restart an external transcriber from the beginning; byte-level transcription resume is not required. Retry/duplicate delivery must not duplicate nodes or published output attachments. Stage outputs and validate them before publication. Cancel must terminate the actual owned process where supported and cleanly mark unpublished work.

Imported transcripts, documents, filenames, and media-derived text are untrusted content, not agent instructions. Tool authorization is enforced server-side. Use argument-safe subprocess calls, bounded source access, and operation-owned temporary directories; do not execute shell text copied from a transcript.

## 8. Collaboration, comments, permissions, and recovery

### 8.1 Native comments and edits

- Selected-node comments live in that node's existing journal/comment facility, including replies and edit/delete rules. Reuse artifact-specific comment semantics when the selected node is an artifact.
- Meeting-wide comments live in the meeting journal. Fragment comments additionally reference stable source anchors; do not copy the same comment into several histories. A node comment can carry an optional meeting anchor and appear in both relevant projections with one identity.
- Attribute author, actor, and time on the server. Show agent changes as agent changes, including the initiating user/run where available. Preserve existing comment history and expected-body concurrency behavior.
- Native node editors and transcript/annotation editors must use expected revisions. On conflict retain the local draft and show the current server version with reload/reapply/compare actions. Do not silently reset the draft to the fetched document or overwrite another member's edits.
- Simple optimistic concurrency is sufficient. Co-editing, not keystroke-level simultaneous rich-text collaboration, is required. No CRDT or operational-transform subsystem.
- Publish scoped events for meeting metadata, links, attachments, comments, and processing state; reuse the existing event/refresh seam. On reconnect, refetch authoritative snapshots/revisions. Do not use full-media polling or client local storage as shared truth.

### 8.2 Authorization contract

The current roles are not proof that this feature is authorized: existing task/comment/session/artifact permissions differ, and an existing “editor” does not automatically have generic node write access. Add explicit project-scoped grants or map to existing actions only where semantics truly match. Update server authorization, capability responses, route navigation, and controls together.

Required capabilities and combinations:

| Action | Required authority |
| --- | --- |
| List/open meeting and its sources | Meeting read plus permission to read each exposed node/attachment. Filter inaccessible linked metadata and backlinks. |
| Create/edit/archive meeting | Meeting write; no implicit ability to edit unrelated node types. |
| Upload/replace/annotate attachment | Attachment write on its owner; access to every reused input source. |
| Link existing node / change relationship | Meeting-link write and permission to read target/source. Target body editing is a separate capability. |
| Create node from meeting | Meeting-link write plus native creation permission for the selected target type. |
| Edit selected node | That node type's ordinary write capability, regardless of the page used. |
| Comment | Comment capability for the owning node/meeting and read access to any attached source anchor. |
| Launch/interact with agent | Explicit session/agent execution authority; never implied by comment or node-edit authority. |

Do not silently broaden all existing member roles. Provide explicit grants for trusted team members to perform the required create/edit/comment workflows; test them as non-admin users. Read-only users can review permitted materials but cannot mutate through hidden APIs. Users who can see an output node but not its source meeting must not gain the recording through a backlink or screenshot lineage URL.

Native chat currently supports powerful host execution. Prompt scoping alone cannot confine filesystem or shell access. Either use a verified restricted execution mode with actor-bound tool authorization, or require an explicit trusted-host-agent execution grant. Do not present ordinary editor access as a secure sandbox for an unrestricted host agent. Authorized team members may use chat through that explicit grant; do not solve security by silently omitting team chat from the product.

Enforce the same policy on JSON APIs, upload chunks/finalization, media byte ranges, downloads, CLI/tools, events, run interaction, and derived artifact/image rendering. Follow existing cookie/CSRF/origin policy and add tests for new binary routes; CORS must not become wildcard credential access. Redact unauthorized IDs/titles, not just the underlying file bytes.

### 8.3 Deletion and interrupted work

Archive is the normal meeting removal action. Destructive deletion, if exposed, requires an explicit impact summary and normal permissions; it never cascades to produced nodes. Keep historical source references meaningful, showing unavailable content if a user deliberately removes it. Removing one attachment reference must not delete a shared payload still in use elsewhere.

Offline edits remain visibly unsaved. Reconnect retries use stable request IDs; changes are not silently dropped when a token expires. Upload interruption resumes from the server-confirmed offset. A failed derivative operation leaves earlier usable revisions available. No destructive cleanup of originals or automated orphan-blob deletion is required for V1.

## 9. Implementation seams and current-source map

Prefer extending the existing shared services. New meeting-specific UI and source/alignment logic are justified; a parallel node database, chat stack, auth stack, transport, or task editor is not.

| Area | Existing code to inspect/reuse | Required change or trap |
| --- | --- | --- |
| Node descriptors/kernel | [`node_type.rs`](crates/orgasmic-core/src/node_type.rs), [`node_kernel.rs`](crates/orgasmic-core/src/node_kernel.rs), [`node_types.rs`](crates/orgasmic-daemon/src/node_types.rs), [`shipped/schema/node-types/`](shipped/schema/node-types/) | Add meeting descriptor, reserved extras, registration/scaffold/load support. A descriptor alone is insufficient. |
| Node resolution | [`node_kind.rs`](crates/orgasmic-core/src/node_kind.rs), [`id.rs`](crates/orgasmic-core/src/id.rs), [`api.rs`](crates/orgasmic-daemon/src/api.rs), [`node.rs`](crates/orgasmic-cli/src/node.rs) | `NodeKind`, CLI kind selection, and `NodeLayer` contain fixed lists; `NodeLayer::for_id` otherwise falls through to glossary. Resolve registered nodes explicitly and preserve parity/tests. |
| Index/graph/backlinks | [`index.rs`](crates/orgasmic-daemon/src/index.rs), live `conventions/decision-graph.org` | Current projections specialize tasks/artifacts/graph. Index meetings, canonical links, accessible node summaries, and reverse backlinks; reuse the closed edge vocabulary. |
| Writer | [`writer.rs`](crates/orgasmic-daemon/src/writer.rs) | Reuse serialized mutations, request IDs, and replay; put revision checks and create/link recovery at the write boundary. |
| Native editing | [`NodeDocEditor.tsx`](ui/src/components/orgdoc/NodeDocEditor.tsx), [`TaskDialog.tsx`](ui/src/components/TaskDialog.tsx), [`orgNodes.ts`](ui/src/components/node-views/orgNodes.ts) | Extract/reuse existing detail content only as needed. Preserve drafts on conflict; the current document refresh effect can reset them. Do not extend the decision/glossary-only UI type union as if it were a universal node resolver. |
| Artifacts | [`artifacts.rs`](crates/orgasmic-daemon/src/artifacts.rs), [`ArtifactView.tsx`](ui/src/components/ArtifactView.tsx), [`ArtifactRenderer.tsx`](ui/src/lib/artifacts/ArtifactRenderer.tsx), [`Image.tsx`](ui/src/lib/artifacts/blocks/Image.tsx) | Reuse artifact lifecycle/comments and safe rendering. Add managed asset resolution; attachment files are not automatically MDX artifact nodes. |
| Chat | [`runDock.tsx`](ui/src/lib/runDock.tsx), [`RunDock.tsx`](ui/src/components/manager/RunDock.tsx), [`ChatSetup.tsx`](ui/src/components/manager/ChatSetup.tsx), [`RunSurface.tsx`](ui/src/components/manager/RunSurface.tsx), chat launch handlers in `api.rs` | Current pinned chat is project-scoped. Add durable meeting/conversation scope and explicit focus context without replacing the native chat. |
| Auth/transport/events | [`auth.rs`](crates/orgasmic-daemon/src/auth.rs), [`authz.rs`](crates/orgasmic-daemon/src/authz.rs), [`transport.ts`](ui/src/lib/transport.ts), [`capabilities.ts`](ui/src/lib/capabilities.ts), [`events.rs`](crates/orgasmic-daemon/src/events.rs) | New node write/link/attachment permissions, binary/range support, scoped media access, and event refresh all need end-to-end treatment. |
| Navigation/design | [`router.tsx`](ui/src/app/router.tsx), [`AppShell.tsx`](ui/src/components/AppShell.tsx), [`styles.css`](ui/src/styles.css), [`ui/package.json`](ui/package.json) | Add native routes/navigation and responsive composition; use installed primitives, styles, and drawing tools. |

Implement a small shared source layer for attachment references, anchors, alignment, and authorized source reads. Keep file transfer/storage mechanics behind the attachment service and processing execution behind existing run services. Meeting commands orchestrate ordinary node services; they must not contain a second implementation of every node type's rules. Avoid a speculative plugin/provider framework for one initial media engine.

### 9.1 Proposed API/CLI surface

These names are illustrative **new** contracts; reconcile naming with existing routes/CLI before exposing them. Existing native node/artifact edit and generation endpoints remain the canonical mutation paths.

| Contract | Minimum input/output |
| --- | --- |
| List/create/read/edit meetings | Project, ordinary node metadata, expected revision; meeting summaries/detail and permitted actions. |
| Search/resolve node references | Project, query/type filters or IDs; registry-derived type, title, supported actions. Avoid loading every body to populate a picker. |
| Create-from-meeting | Meeting, native node type/payload, anchors, request ID; native node ID, link ID, committed revisions/transaction. |
| Add/edit/remove meeting link | Meeting/target, relationship, anchors, expected link revision, request ID; authoritative link and revisions. |
| Read node meeting sources | Target node; permission-filtered backlinks and source navigation data. |
| Attachment upload/revision/download | Owner node, upload/session/revision IDs, bounded binary chunks; metadata, confirmed offset, authorized content access. |
| Edit transcript/alignment/annotation | Named source revision, validated edit data, expected current revision, request ID; new revision and export/playback mapping. |
| Start/read/cancel processing | Meeting, operation kind, input revisions, parameters, request ID; run/operation identity, progress, output refs, error. |
| Launch/resume meeting chat | Existing native provider options plus durable scope/conversation identity; existing run/session contract. |
| Add/read comments | Existing node comment command plus optional validated meeting/source anchor. |

Expose corresponding CLI/tool operations for attachment ingestion, meeting/link reads/writes, create-from-meeting, and media processing; agents must not hand-write `links.org` or other ledger state. Human UI and agents must share validation/idempotency/authorization. Publish precise errors: invalid range, unavailable mapping, missing tool, conflict, denied action, unsupported cross-project reference, or missing source—with the corrective action where safe.

## 10. Delivery plan

Implement in dependency order. The milestones are engineering slices, not permission to call an attachment-only or task-only page the finished feature. Keep all new write/media surfaces protected from their first exposure.

### M0 — Revalidate and establish fixtures

Read current repo/ledger rules and inspect the source map against HEAD. Confirm the node resolver, writer guarantees, auth model, and native chat persistence. Create a small isolated test project with two recordings containing an intentional gap, a transcript, and several node types. Do not use production recordings or mutate the user's live Orgasmic ledger for routine tests.

Exit: concrete affected contracts and test fixtures are identified; any necessary deviation from this spec is explained, not silently substituted. No production daemon restart or provider billing is needed.

### M1 — Meeting identity, generic links, and authorized writes

Add the descriptor and registry/resolver support; meeting CRUD through daemon/CLI; canonical link/anchor parsing; node-generic search and backlinks; graph projection; role/capability checks; journal attribution; serialized revision validation and idempotent create-and-link. Exercise a registered custom node type as well as task/decision/term/artifact. Avoid a wholesale node-kernel rewrite.

Exit: CLI/integration tests can create a meeting, link an existing node, create a real product, reopen/reindex, and see the same relationship from both ends. Retry/restart cannot duplicate the product; denied users cannot perform the write. Unknown IDs never fall into the glossary writer.

### M2 — Managed attachments and remote-safe media

Implement the shared attachment manifest/store, resumable ingestion, immutable revisions, authorized metadata/download/range routes, browser/native transport support, safe source probing, and explicit missing/unsupported states. Include generic node attachments, not a meeting-only folder uploader. Define a recoverable export/backup path including payloads.

Exit: a non-admin authorized tablet/browser user can upload, interrupt/resume, play, and seek a large recording without full-file client buffering. Another unauthorized member cannot read the manifest, URL, byte range, or event. Restart and missing-blob cases report correctly.

### M3 — Native meeting workspace and manual collaboration

Add Meetings navigation/index/detail routes. Compose the node list, player, transcript/files placeholder surfaces, native detail/editor, comments, source anchors, create/link actions, deep links, and reverse navigation. Add responsive pane behavior and draft-preserving conflicts. Stream/refetch changes through existing events.

Exit: two members can create/edit/link ordinary nodes and comment in a meeting; the normal task/decision/term/artifact views show the same objects. Each can click multiple recording anchors; a competing edit cannot silently lose either draft. The tablet layout is usable before agent/media automation is added.

### M4 — Editable transcripts, alignment, and annotated frames

Implement transcript/SRT/VTT import, stable cues and revisions, cue editing/split/merge, source-clock mappings with cuts, playback highlighting, subtitle export, current-time/range anchoring, frame capture, and editable annotations with rendered derivatives. Reuse native media controls and installed drawing tools.

Exit: the known split-timeline fixture resolves correctly; a cue spanning a cut yields split/unavailable fragments rather than a guessed seek. Subtitle edits export/reimport correctly. Old anchors and annotation originals remain usable after edits and reloads.

### M5 — Native scoped chat and media/node tools

Extend chat scope and durable conversation association; add visible per-message context; wire inspect/transcribe/align/mix/subtitle/capture/create/link operations to real daemon commands and existing run supervision. Reuse native artifact generation. Deliver one functioning configured transcription path and explicit capability/setup errors. Enforce trusted execution grants independently of node editing.

Exit: in a native meeting conversation, an authorized user can request an actual transcription, subtitles, and native tasks/decisions/terms or an artifact with source links. Reopening restores context/history. Changing meetings does not retarget a run. Cancellation, failure, and retry are observable and preserve prior outputs.

### M6 — Recovery, security, and regression gate

Run the acceptance matrix below, including non-admin roles, raw endpoint access, revoked media access, token expiry, reconnect, concurrent writes, upload/process interruption, generic nodes, original-source preservation, and export/restore. Fix shared-seam failures where they originate. Exercise existing node editors, artifact generation, project chat, and boards for regressions.

Exit: automated checks and targeted browser/tablet evidence cover the required behavior. No admin-only demo, fake progress, duplicate node state, or unsecured media route remains.

### M7 — Import the real Max meeting and hand off

Use the migration rules in section 12. First run a read-only inventory/dry run, then import into the user-selected project only when authorized. Preserve current edits and source mappings; verify representative beginning/middle/end anchors and annotated tasks. Make the import idempotent and report every imported/skipped/unavailable item.

Exit: the Max meeting appears under Meetings with working sources, editable sidecars, native meeting chat, current native nodes, comments/annotations where present, and correct timecode navigation. Re-running import creates no duplicates. Document any unavailable source explicitly rather than passing incomplete work as complete.

## 11. Acceptance and verification

Use focused unit/integration tests for pure mapping and persistence logic; browser tests/manual device checks for playback, auth transport, focus, and touch. Prefer the existing test tooling. Test fixtures must not contain private meeting recordings or credentials.

| Test | Pass condition |
| --- | --- |
| A01 · R1 | Meeting create/edit/archive survives daemon restart and full reindex; descriptor, CLI help/kind handling, and node lookup agree. |
| A02 · R2 | Video-only, audio-only, transcript-only, and mixed-source meetings work without fabricated assets/times. |
| A03 · R5–R7 | One task has several fragments in one meeting and links from two meetings; edits are shared, links remain independent. |
| A04 · R6 | Task, decision, glossary, artifact, another meeting, and a registered custom node can be linked without a hard-coded four-type picker. Unsupported edit/create actions are disabled accurately. |
| A05 · R5–R6 | Create-from-meeting makes a native node and Produced link; link-existing defaults to Related. Changing relation is explicit and attributed. |
| A06 · R6–R7 | Backlinks/graph projections match canonical link records after editing/removing/reindexing. Unlink/archive does not delete products. |
| A07 · R7 | Several recordings, points/ranges, transcript selections, untimed text excerpts, and meeting-wide links navigate correctly. |
| A08 · R7/R9 | Gap, cut, drift, exact boundary, out-of-duration, and cross-cut mapping tests pass. Unmapped content is reported, not clamped to a misleading timestamp. |
| A09 · R9 | Cue text/speaker/time edits and split/merge preserve source revisions/citations; valid SRT/VTT exports match selected playback clock. |
| A10 · R3/R9 | Frame capture provenance is correct; annotations reopen editable; originals survive; exported annotated PNG matches saved geometry. |
| A11 · R3/R8 | Native fields/lifecycle/comments edited here appear in normal node views and vice versa; there is no meeting-local body copy. |
| A12 · R8 | Meeting, fragment, and node comments have correct ownership, replies, edit/delete behavior, actor attribution, and visibility. |
| A13 · R8 | Two concurrent edits trigger a visible conflict where necessary; refresh/reconnect never silently destroys the losing draft. |
| A14 · R4 | Chat is the native provider/run surface, persists by meeting/conversation, and restores history after reload/new run. |
| A15 · R4 | Sent focus context is immutable; switching node/source/meeting afterward does not retarget an in-flight request. |
| A16 · R4–R6 | Real configured transcription and subtitle generation produce attachments; extraction produces normal nodes with traceable anchors; artifact creation follows the native artifact contract. |
| A17 · R4/R8 | Analysis-only requests do not create nodes; extraction does not start task execution; authorized reconciliation updates current native nodes in place; unsupported inferences are marked rather than treated as agreed facts. |
| A18 · R4/R9 | Failed/cancelled/interrupted processing reports reality, retains originals/prior outputs, and can retry without duplicate nodes or attachments. |
| A19 · R5/R8 | Crash/retry around native-node creation, link write, and reply delivery converges on one complete create-from-meeting result. Test each interruption boundary. |
| A20 · R2/R10 | Large upload uses bounded memory, resumes from confirmed offset, rejects wrong offsets/checksums, and never publishes a partial asset as ready. |
| A21 · R10 | Web and native remote profiles play/seek via authorized range requests without exposing global bearer tokens or buffering the entire source. |
| A22 · R8/R10 | Reader, commenter, explicit node editor/creator, and trusted agent operator are tested as non-admin identities. UI and raw endpoints enforce the same permissions. |
| A23 · R8/R10 | Revocation/expiry applies to metadata, chunks, content ranges, events, backlinks, screenshots, and session interaction; no cross-project or unauthorized title/ID leakage. |
| A24 · R4/R10 | Transcript prompt injection, path traversal filenames, malformed media/subtitles, unsafe uploaded active content, and unsafe subprocess arguments cannot bypass the intended boundaries. |
| A25 · R9/R10 | Missing blobs, offline saves, token expiry, and event reconnect show truthful recovery states; export/restore includes referenced assets and preserves IDs/links. |
| A26 · R3/R10 | Desktop and tablet portrait/landscape support seeking, editing, create/link, comments, chat, and annotation. Drafts/playback survive pane changes; keyboard focus and touch controls work. |
| A27 · R1–R10 | Max import uses current reconciled data, preserves provenance/edits, maps sample anchors correctly, and is idempotent. |
| A28 · Regression | Existing board/task views, ordinary node editing, artifact rendering/comments/generation, project chat, and member auth continue to work. |

Implementation verification commands, run from the appropriate directory:

```sh
# Repository root
cargo build
scripts/run-tests.sh
cargo clippy --workspace --all-targets

# ui/
npm run typecheck
npm test
npm run build
```

Follow the live verification convention, including classification/re-run of an inconclusive test run. Do not substitute `cargo test --workspace` for the repository runner on this Mac or call a billed provider test without explicit authorization. Keep tests isolated from the installed production daemon and user's live ledger. Report what ran, actual results, and any missing device/provider validation; do not call untested manual behavior verified.

## 12. First migration: Max / ORSL, 2026-09-04

Local source directory at the time of writing: `/Users/aspirational/Desktop/ORSL call 2026-09-04`. It is an import fixture/source of user content, not a runtime dependency for the finished feature. The implementing agent should inventory current files before assuming every optional derivative exists.

Important source material:

- `Interactive/data/tasks.json`, per-task Markdown under `Interactive/data/tasks/`, `Interactive/data/attachments/`, and `Interactive/data/history/`: current reconciled task content, images, and edit history. **Import current data, not `seed.json`.** The known working set contains 21 corrected tasks; preserve IDs through an import mapping rather than pretending the old local numbers are native Orgasmic IDs.
- `Interactive/data/subtitles.vtt` and available synchronized `.srt`/`.ass` sidecars: preserve as originals/imported revisions and map them to the correct playback clock.
- `Synced/ORSL-2026-09-04-browser.mp4`: browser-playable combined-voice recording, approximately 5,791.458 seconds. Keep separate original recordings/audio and any alternative synchronized container/tracks. The sync manifest also references the original video outside this folder, `/Users/aspirational/Desktop/CleanShot 2026-09-04 at 21.02.40.mp4`, and `work/plaud_audio.mp3`; inventory these explicitly rather than assuming the folder is self-contained.
- `work/plaud_share.json`: original Plaud transcript is under `data_file.trans_result`, with source-clock millisecond timings and speakers. Keep the original rather than replacing it with edited subtitle text.
- `Synced/sync-info.json`, `work/sync_anchors.json`, alignment checks, and verification notes: inspect current content and use it to validate synchronization rather than inferring one global offset. The existing subtitle generation split long utterances proportionally; it is not word-level forced alignment. Preserve that limitation in source metadata rather than implying every word was precisely timed.
- Original task files/images and `REVIEW.md`: retain as import provenance. The reconciled set includes the previously missing destination-search task; an unrelated image from original task 20 must not be silently attached to that task again.

Known Plaud-to-video mapping, in seconds, to verify against the actual source revisions:

```text
Plaud < 174.87                 → not present in the video
174.87 ≤ Plaud < 1520.87       → video = Plaud − 174.87
1520.87 ≤ Plaud < 1878.68      → not present: removed pause/content
Plaud ≥ 1878.68               → video = Plaud − 532.68
```

Apply the final segment only while corresponding video content exists. The join is at video `1346.00`; `1520.87` is in the unavailable interval and `1878.68` maps to the join. Test these boundaries explicitly. Do not add an offset to already-video-timed task anchors or subtitle cues a second time.

The importer must:

1. Dry-run and report detected sources, current task count, transcript clocks, attachments, edits/history, unsupported fields, and intended mappings before mutation.
2. Use a stable import identity plus per-item external keys to map local task numbers/files to native nodes and attachment revisions. Persist the mapping through supported commands, without a parallel editable task database.
3. Create one Meeting and real task products from the current reconciled content. Do not translate “not reviewed” or another standalone review marker into a conflicting task lifecycle. Preserve it as import/review metadata or an import note if useful, while assigning a valid native initial lifecycle.
4. Preserve source text, corrections, current descriptions/criteria, selected images, annotations, and available edit history. Historical imported authors/times are labeled as imported provenance; the importer must not impersonate those users as authenticated writers.
5. Import current timecodes with their actual source clocks and piecewise alignment. Carry original and corrected transcript/subtitle revisions separately.
6. Never overwrite subsequent native edits merely because the import is rerun. Report changed source content as an update requiring an explicit policy; identical input is a no-op.
7. Return a reconciliation report: imported native IDs, links/fragments, attached originals/derivatives, preserved history, skipped/unavailable items, and sample seek verification.

Do not copy private recordings into the source repository, embed temporary tunnel credentials/URLs, delete the standalone application, or assume this planning request authorizes importing into the user's live project. Keep the old material intact until the user verifies the native result.

## 13. Scope boundaries and completion

Included in the first complete feature: first-class meetings; all-node links/products; native editing/comments/chat; managed attachments; real configured media operations; editable sidecars and annotations; source clock correctness; manual creation/linking; multiuser permissions/conflicts; remote tablet playback; and the verified migration path.

Explicitly not required: live meeting capture/bots, calendar integrations, diarization research, a new general workflow engine, another graph edge vocabulary, automatic execution of extracted tasks, mandatory proposal approval, CRDT editing, public unauthenticated sharing, cloud object-storage adapters, cross-project meeting links, or automatic destructive asset cleanup. Add those only for an actual follow-up requirement.

Do not stop at a visual mockup, task-only extraction, an artifact containing a video, or chat buttons without executable operations. Completion means the meeting is a normal persistent Orgasmic node and the workspace operates on the same native nodes, attachments, permissions, comments, and agent sessions used by the rest of the product.

The implementing agent's final handoff should include the code/test results, the enabled media engine and setup requirements, permission choices, known limitations, migration dry-run/result where authorized, and a short walkthrough of creating, reviewing, and returning to a node through its meeting source.
