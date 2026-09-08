# Meetings

Import UTF-8 notes, edit with version checks, upload recordings, and link their
timestamps to tasks. The importer needs Python 3. Sidecars and chat are deferred.

```sh
orgasmic plugin add /absolute/path/to/examples/plugins/meetings
orgasmic plugin check meetings
orgasmic plugin enable meetings --project PROJECT
orgasmic plugin run meetings import --project PROJECT -- /path/to/notes.txt --title 'Planning'
```

Approve the displayed capabilities only if you trust this code. Open Meetings
in the project navigation. Edit the **installed** `ui/index.js` under
`~/.orgasmic/user/plugins/meetings`: the view reloads within a few seconds while
SDK-held drafts survive. `plugin add` copies files, it is not a symlink.
Disabling the plugin removes its custom view; an open collection falls back
to the generic read-only list without deleting its nodes.

Open a meeting and choose an audio/video file (up to 8 GiB). Uploads use 4 MiB
checksummed chunks. Pause or retry with the same file to resume from the daemon's
confirmed offset, including after restarting the app. Native media controls
support seeking without downloading the full file. Choose a task and click
“Link current time”; the task's Backlinks section opens this recording at that
timestamp. Each link retains the exact immutable recording revision.

Updating an older installation requires re-approval of `links.read`,
`links.write`, `attachments.read`, and `attachments.write`. UI code executes as
the signed-in user. Commands are separately capability-scoped.

Recordings stay outside Git in `~/.orgasmic/assets/<sha256-project-id>/`.
Back up that directory separately: a cloned ledger contains metadata, not media.
Moving or renaming the project folder does not move its asset store.
The local quota is 64 GiB including reserved uploads, with 4096 unfinished uploads.
Finished receipts do not consume upload slots. Native playback uses the existing
same-origin session cookie, not a separate expiring media token.
Completed recordings are retained. Supported containers: WAV/MP3/Ogg/MP4/WebM;
the browser must also support the codec. Convert unsupported recordings before
uploading. This example neither transcodes nor synchronizes assets.

## Verification

```sh
RUST_TEST_THREADS=1 cargo test -p orgasmic-daemon --test node_services_routes
RUST_TEST_THREADS=1 cargo test -p orgasmic-daemon --test node_services_routes two_gib_recording_upload_and_seek -- --ignored
cd ui && npx vitest run src/components/__tests__/MeetingsPlayer.test.jsx src/components/__tests__/NodeBacklinks.test.tsx src/lib/__tests__/nodeServices.test.ts
```

The explicit large-file gate transfers 2,147,483,648 bytes over HTTP in 4 MiB
chunks, verifies the content digest, and checks range reads and indexed anchors.
It needs several GiB of free temporary disk; debug-mode hashing can take minutes.

For browser verification, build the UI and run that large-file test with
`ORGASMIC_EMBED_UI=1 ORGASMIC_UI_PREBUILT=1 ORGASMIC_SERVICE_BROWSER_SMOKE=1`.
It prints the path of a private `browser-session.json` fixture containing a
one-use login URL (do not log or share it). Open that session, then
`/projects/demo/nodes/meetings`. Open the meeting, seek to 1:30, select the
task and link the current time. Open the task, click its 1:30 backlink, and
verify playback returns to 90 seconds. Check an additional upload through the
file input. Only after those assertions pass, create `browser-done` alongside
the fixture file; the test shuts down its daemon and removes its temporary
assets. Interrupting is a failed browser verification, not a pass.

Verified on 2026-09-08: the 2 GiB recording played and sought to 90 seconds;
the task backlink returned to that exact revision/time; an 8 MiB browser upload
played successfully. No console errors or horizontal overflow at 390 px.
