# Meetings-lite

Nodes only: import UTF-8 notes, browse meetings, and edit title/body with version
checks. No media, links, sidecars, or chat yet. The importer needs Python 3.

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
