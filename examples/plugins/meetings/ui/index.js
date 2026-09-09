import { createElement as h, useEffect, useId, useState } from 'react';
import { Button, Input, Textarea, useEventStream, useMe, useResource } from '@orgasmic/plugin-sdk';
import { Player } from './player.js';

const MAX_NOTES_BYTES = 1024 * 1024;

async function readNotes(file) {
  if (file.size > MAX_NOTES_BYTES) throw new Error('Notes files must be 1 MiB or smaller.');
  return file.text();
}

export function register(ctx) {
  ctx.registerStyles(`
    .meetings { display: grid; gap: 1rem; }
    .meetings h1 { font-size: 1.5rem; font-weight: 600; }
    .meetings ul { display: grid; gap: .5rem; }
    .meetings label { display: grid; gap: .375rem; font-size: .875rem; }
    .meetings .muted { color: var(--muted-foreground); font-size: .875rem; }
    .meetings .error { color: var(--destructive); }
    .meetings textarea { min-height: 16rem; }
    .meetings h2 { font-weight: 600; }
    .meetings audio, .meetings video { width: 100%; max-height: 24rem; }
    .meetings select { min-height: 2.75rem; border: 1px solid var(--border); border-radius: var(--radius); padding: .5rem; background: var(--background); color: var(--foreground); max-width: 100%; }
    .meetings .recording { border-bottom: 1px solid var(--border); padding-bottom: 1rem; }
    .meetings .upload-progress { display: flex; flex-wrap: wrap; align-items: center; gap: .5rem; }
    .meetings .meeting-row { width: 100%; min-height: 2.75rem; justify-content: start; white-space: normal; overflow-wrap: anywhere; height: auto; text-align: start; }
    .meetings .meeting-header { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: .75rem; }
    .meetings .meeting-empty { display: grid; gap: .5rem; border: 1px dashed var(--border); border-radius: var(--radius); padding: 1rem; }
    .meetings .meeting-empty h2, .meetings .new-meeting h2 { font-size: 1rem; font-weight: 600; }
    .meetings .new-meeting { display: grid; gap: 1rem; max-width: 48rem; border: 1px solid var(--border); border-radius: var(--radius); padding: 1rem; }
    .meetings .form-actions { display: flex; flex-wrap: wrap; gap: .5rem; }
  `);
  return ctx.registerNodeView('meetings', function Meetings({ nodeId, projectId, onOpenNode }) {
    return nodeId ? h(Detail, { nodeId, projectId, onOpenNode }) : h(List, { onOpenNode });
  });

  function List({ onOpenNode }) {
    const { can } = useMe();
    const result = useResource(`meetings:${ctx.projectId}`, () => ctx.get('/graph/nodes?layer=meetings'));
    const [creating, setCreating] = useState(false);
    useEventStream((event) => {
      if (event.topic === 'graph' && event.payload.project_id === ctx.projectId && event.payload.layer === 'meetings') void result.refresh();
    });
    const writable = can(ctx.projectId, 'nodes.write');
    return h('section', { className: 'meetings', 'aria-label': 'Meeting notes' },
      h('div', { className: 'meeting-header' }, h('h1', null, 'Meeting notes'),
        writable && !creating ? h(Button, { type: 'button', onClick: () => setCreating(true) }, 'New meeting') : null),
      h('p', { className: 'muted' }, 'Notes and decisions from your conversations.'),
      creating ? h(NewMeeting, { onCancel: () => setCreating(false), onCreated: async (id) => { await result.refresh(); onOpenNode(id); } }) : null,
      result.loading ? h('p', { role: 'status' }, 'Loading meetings…') : null,
      result.error ? h('p', { role: 'alert', className: 'error' }, String(result.error)) : null,
      result.data?.length === 0 && !creating ? h('div', { className: 'meeting-empty' },
        h('h2', null, 'Capture your first meeting'),
        h('p', { className: 'muted' }, writable
          ? 'Create it here, then add a recording and link important moments to tasks.'
          : 'Meeting notes and recordings will appear here.')) : null,
      h('ul', null, (result.data ?? []).map((node) => h('li', { key: node.id },
        h(Button, { variant: 'outline', className: 'meeting-row', onClick: () => onOpenNode(node.id) }, node.title || node.id)))));
  }

  function NewMeeting({ onCancel, onCreated }) {
    const titleId = useId();
    const notesId = useId();
    const fileId = useId();
    const [title, setTitle] = useState('');
    const [notes, setNotes] = useState('');
    const [saving, setSaving] = useState(false);
    const [error, setError] = useState('');
    async function importNotes(event) {
      const file = event.target.files?.[0];
      if (!file) return;
      setError('');
      try {
        setNotes(await readNotes(file));
        if (!title.trim()) setTitle(file.name.replace(/\.[^.]+$/, '') || 'Meeting');
      } catch (cause) { setError(String(cause)); }
      event.target.value = '';
    }
    async function create(event) {
      event.preventDefault();
      if (saving || !title.trim()) return;
      if (new Blob([notes]).size > MAX_NOTES_BYTES) { setError('Notes must be 1 MiB or smaller.'); return; }
      setSaving(true); setError('');
      try {
        const created = await ctx.post('/org/node', { kind: 'meetings', title: title.trim(), body: notes,
          request_id: crypto.randomUUID() });
        await onCreated(created.id);
      } catch (cause) { if (!ctx.signal.aborted) setError(String(cause)); }
      finally { setSaving(false); }
    }
    return h('form', { className: 'new-meeting', onSubmit: create },
      h('div', null, h('h2', null, 'New meeting'), h('p', { className: 'muted' }, 'Start with a title and notes. Add the recording on the meeting screen.')),
      h('label', { htmlFor: titleId }, 'Title', h(Input, { id: titleId, value: title, required: true, autoFocus: true, disabled: saving,
        onChange: (event) => setTitle(event.target.value) })),
      h('label', { htmlFor: notesId }, 'Notes', h(Textarea, { id: notesId, value: notes, maxLength: MAX_NOTES_BYTES, disabled: saving,
        placeholder: 'Paste a transcript, summary, decisions, or follow-ups…', onChange: (event) => setNotes(event.target.value) })),
      h('label', { htmlFor: fileId }, 'Import notes from a text file', h(Input, { id: fileId, type: 'file', accept: '.txt,.md,text/plain,text/markdown', disabled: saving, onChange: importNotes })),
      error ? h('p', { role: 'alert', className: 'error' }, error) : null,
      h('div', { className: 'form-actions' },
        h(Button, { type: 'submit', disabled: saving || !title.trim() }, saving ? 'Creating…' : 'Create meeting'),
        h(Button, { type: 'button', variant: 'outline', disabled: saving, onClick: onCancel }, 'Cancel')));
  }

  function Detail({ nodeId, projectId, onOpenNode }) {
    const { can } = useMe();
    const titleId = useId();
    const notesId = useId();
    const notesFileId = useId();
    const result = useResource(`meeting:${projectId}:${nodeId}`, () => ctx.get(`/org/node?id=${encodeURIComponent(nodeId)}`));
    const [draft, setDraft] = useState(() => ctx.getDraft(nodeId));
    const [saving, setSaving] = useState(false);
    const [message, setMessage] = useState('');
    const [error, setError] = useState('');
    useEffect(() => {
      if (result.data && !draft) setDraft({ title: result.data.title, body: result.data.body, base_version: result.data.source.base_version });
    }, [result.data, draft]);
    const writable = can(projectId, 'nodes.write') && Boolean(result.data) && result.data.schema_matches !== false;
    const change = (key, value) => {
      const next = { ...draft, [key]: value };
      ctx.setDraft(nodeId, next);
      setDraft(next);
      setMessage('Unsaved changes');
    };
    async function appendNotes(event) {
      const file = event.target.files?.[0];
      if (!file || !draft) return;
      setError('');
      try {
        const imported = await readNotes(file);
        change('body', [draft.body.trimEnd(), imported].filter(Boolean).join('\n\n'));
      } catch (cause) { setError(String(cause)); }
      event.target.value = '';
    }
    async function save(event) {
      event.preventDefault();
      if (!writable || !draft || saving) return;
      setSaving(true);
      setError('');
      try {
        const saved = await ctx.post(`/org/node/${encodeURIComponent(nodeId)}/edit?json=true`, { kind: 'meetings', project: projectId,
          request_id: crypto.randomUUID(), base_version: draft.base_version,
          ops: [{ op: 'set_title', title: draft.title }, { op: 'set_body', body: draft.body }] });
        ctx.clearDraft(nodeId);
        setDraft({ title: saved.title, body: saved.body, base_version: saved.source.base_version });
        setMessage('Saved');
      } catch (cause) {
        if (!ctx.signal.aborted) setError(`${String(cause)} Your draft is kept. Copy it before reopening if another edit changed the version.`);
      } finally { setSaving(false); }
    }
    if (result.error) return h('p', { role: 'alert' }, String(result.error));
    if (!draft) return h('p', { role: 'status' }, 'Loading meeting…');
    return h('div', { className: 'meetings' }, h(Player, { ctx, nodeId, onOpenNode, writable }), h('form', { className: 'meetings', onSubmit: save },
      h('label', { htmlFor: titleId }, 'Title', h(Input, { id: titleId, value: draft.title, required: true, disabled: !writable || saving, onChange: (e) => change('title', e.target.value) })),
      h('label', { htmlFor: notesId }, 'Notes', h(Textarea, { id: notesId, value: draft.body, disabled: !writable || saving, onChange: (e) => change('body', e.target.value) })),
      writable ? h('label', { htmlFor: notesFileId }, 'Append notes from a text file', h(Input, { id: notesFileId, type: 'file', accept: '.txt,.md,text/plain,text/markdown', disabled: saving, onChange: appendNotes })) : null,
      error ? h('p', { role: 'alert', className: 'error' }, error) : null,
      h('p', { role: 'status', className: 'muted' }, writable ? message : 'Read only'),
      writable ? h(Button, { type: 'submit', disabled: saving || !draft.title.trim() }, saving ? 'Saving…' : 'Save notes') : null));
  }
}
