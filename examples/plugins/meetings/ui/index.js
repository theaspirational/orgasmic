import { createElement as h, useEffect, useId, useState } from 'react';
import { Button, Input, Textarea, useEventStream, useMe, useResource } from '@orgasmic/plugin-sdk';
import { Player } from './player.js';

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
  `);
  return ctx.registerNodeView('meetings', function Meetings({ nodeId, projectId, onOpenNode }) {
    return nodeId ? h(Detail, { nodeId, projectId, onOpenNode }) : h(List, { onOpenNode });
  });

  function List({ onOpenNode }) {
    const result = useResource(`meetings:${ctx.projectId}`, () => ctx.get('/graph/nodes?layer=meetings'));
    useEventStream((event) => {
      if (event.topic === 'graph' && event.payload.project_id === ctx.projectId && event.payload.layer === 'meetings') void result.refresh();
    });
    return h('section', { className: 'meetings', 'aria-label': 'Meeting notes' },
      h('h1', null, 'Meeting notes'),
      h('p', { className: 'muted' }, 'Notes and decisions from your conversations.'),
      result.loading ? h('p', { role: 'status' }, 'Loading meetings…') : null,
      result.error ? h('p', { role: 'alert', className: 'error' }, String(result.error)) : null,
      result.data?.length === 0 ? h('p', null, 'No meetings yet. Import a notes file with orgasmic plugin run meetings import.') : null,
      h('ul', null, (result.data ?? []).map((node) => h('li', { key: node.id },
        h(Button, { variant: 'outline', className: 'meeting-row', onClick: () => onOpenNode(node.id) }, node.title || node.id)))));
  }

  function Detail({ nodeId, projectId, onOpenNode }) {
    const { can } = useMe();
    const titleId = useId();
    const notesId = useId();
    const result = useResource(`meeting:${projectId}:${nodeId}`, () => ctx.get(`/org/node?id=${encodeURIComponent(nodeId)}`));
    const [draft, setDraft] = useState(() => ctx.getDraft(nodeId));
    const [saving, setSaving] = useState(false);
    const [message, setMessage] = useState('');
    const [error, setError] = useState('');
    useEffect(() => {
      if (result.data && !draft) setDraft({ title: result.data.title, body: result.data.body, base_version: result.data.source.base_version });
    }, [result.data, draft]);
    const writable = can(projectId, 'nodes.write') && Boolean(result.data) && result.data.schema_matches !== false;
    // core.chat@1 is optional: an older host has no ctx.openChat.
    const chat = typeof ctx.openChat === 'function' && can(projectId, 'chat.write');
    const change = (key, value) => {
      const next = { ...draft, [key]: value };
      ctx.setDraft(nodeId, next);
      setDraft(next);
      setMessage('Unsaved changes');
    };
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
    return h('div', { className: 'meetings' },
      chat ? h('div', null, h(Button, { type: 'button', variant: 'outline', onClick: () => ctx.openChat({ node: nodeId, purpose: 'meeting' }) }, 'Chat')) : null,
      h(Player, { ctx, nodeId, onOpenNode, writable }), h('form', { className: 'meetings', onSubmit: save },
      h('label', { htmlFor: titleId }, 'Title', h(Input, { id: titleId, value: draft.title, required: true, disabled: !writable || saving, onChange: (e) => change('title', e.target.value) })),
      h('label', { htmlFor: notesId }, 'Notes', h(Textarea, { id: notesId, value: draft.body, disabled: !writable || saving, onChange: (e) => change('body', e.target.value) })),
      error ? h('p', { role: 'alert', className: 'error' }, error) : null,
      h('p', { role: 'status', className: 'muted' }, writable ? message : 'Read only'),
      writable ? h(Button, { type: 'submit', disabled: saving || !draft.title.trim() }, saving ? 'Saving…' : 'Save notes') : null));
  }
}
