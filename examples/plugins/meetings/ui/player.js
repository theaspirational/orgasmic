import { createElement as h, useEffect, useId, useRef, useState } from 'react';
import { Button, Input, useMe, useResource, useEventStream, mediaTime, uploadAttachment } from '@orgasmic/plugin-sdk';

export function Player({ ctx, nodeId, onOpenNode, writable }) {
  const { can } = useMe();
  const fieldId = useId();
  const media = useRef(null);
  const pause = useRef(false);
  const alive = useRef(true);
  const position = useRef(0);
  const hint = new URLSearchParams(window.location.search);
  const hinted = hint.get('media_node') === nodeId;
  const [selected, setSelected] = useState(hinted ? hint.get('media_attachment') || '' : '');
  const [error, setError] = useState('');
  const [mediaError, setMediaError] = useState('');
  const [busy, setBusy] = useState(false);
  const [offset, setOffset] = useState(0);
  const [file, setFile] = useState(null);
  const [target, setTarget] = useState('');
  const [linking, setLinking] = useState(false);
  const [message, setMessage] = useState('');
  const storageKey = `meetings-upload:${ctx.projectId}:${nodeId}`;
  const readMedia = can(ctx.projectId, 'attachments.read');
  const readLinks = can(ctx.projectId, 'links.read');
  const assets = useResource(`recordings:${ctx.projectId}:${nodeId}`, () => ctx.get(`/attachments?node=${encodeURIComponent(nodeId)}`), { enabled: readMedia });
  const links = useResource(`meeting-links:${ctx.projectId}:${nodeId}`, () => ctx.get(`/links?node=${encodeURIComponent(nodeId)}`), { enabled: readLinks });
  const tasks = useResource(`meeting-task-targets:${ctx.projectId}`, () => ctx.get('/graph/nodes?layer=task'), { enabled: can(ctx.projectId, 'graph.read') && can(ctx.projectId, 'links.write') });
  const recordings = (assets.data || []).filter((a) => /^(audio|video)\//.test(a.media_type));
  const recording = recordings.find((a) => a.id === selected) || (!selected ? recordings[0] : undefined);
  const url = recording ? ctx.mediaUrl(nodeId, recording.id, recording.revision) : '';
  const editable = writable && can(ctx.projectId, 'attachments.write');
  // core.chat@1 is optional: an older host has no ctx.openChat.
  const chat = typeof ctx.openChat === 'function' && can(ctx.projectId, 'chat.write');
  useEffect(() => { alive.current = true; return () => { alive.current = false; pause.current = true; }; }, []);
  useEventStream((event) => {
    if (event.topic === 'graph' && event.payload.project_id === ctx.projectId && event.payload.layer === 'node-services') {
      if (readMedia) void assets.refresh();
      if (readLinks) void links.refresh();
    }
  });
  useEffect(() => {
    setMediaError('');
  }, [recording?.id, recording?.revision]);

  function loaded() {
    if (!media.current) return;
    const requested = hinted && hint.get('media_attachment') === recording.id && hint.get('media_revision') === recording.revision
      ? Number(hint.get('media_ms')) / 1000 : position.current;
    if (Number.isFinite(requested) && requested >= 0 && requested <= media.current.duration) media.current.currentTime = requested;
    setMediaError('');
  }
  async function upload() {
    if (!file || busy) return;
    setBusy(true); setError(''); setMessage(''); pause.current = false;
    const signature = `${file.name}:${file.size}:${file.lastModified}`;
    let pending;
    try { pending = JSON.parse(localStorage.getItem(storageKey) || 'null'); } catch { /* storage is optional */ }
    const id = pending?.signature === signature ? pending.id : crypto.randomUUID();
    try { localStorage.setItem(storageKey, JSON.stringify({ id, signature })); } catch { /* this session still works */ }
    try {
      const saved = await uploadAttachment(ctx, nodeId, file, id, (n) => { if (alive.current) setOffset(n); }, () => pause.current);
      try { localStorage.removeItem(storageKey); } catch { /* optional */ }
      if (!alive.current) return;
      position.current = 0; setSelected(saved.id); setFile(null); setMessage('Recording uploaded');
      await assets.refresh();
    } catch (cause) {
      if (alive.current && !ctx.signal.aborted) setError(`${String(cause)} Select the same file and retry to resume from the confirmed offset.`);
    } finally { if (alive.current) setBusy(false); }
  }
  async function linkTime(event) {
    event.preventDefault();
    if (!recording || !media.current || !target || linking) return;
    const start = Math.round(media.current.currentTime * 1000);
    if (!Number.isSafeInteger(start) || !Number.isFinite(media.current.duration) || start > media.current.duration * 1000) return;
    setLinking(true); setError(''); setMessage('');
    try {
      const current = await ctx.get(`/links?node=${encodeURIComponent(nodeId)}&include_deleted=true`);
      const previous = current.find((link) => link.target === target);
      const anchor = { attachment: recording.id, revision: recording.revision, start_ms: start, label: '' };
      await ctx.post('/links', { source: nodeId, target, kind: previous?.kind || 'RELATES_TO',
        base_revision: previous?.revision || 0, request_id: crypto.randomUUID(), anchors: [...(previous && !previous.deleted ? previous.anchors : []), anchor] });
      setMessage(`Linked ${target} at ${mediaTime(start)}`);
      await links.refresh();
    } catch (cause) { if (!ctx.signal.aborted) setError(String(cause)); }
    finally { setLinking(false); }
  }
  // Open the meeting's chat with a range chip at the playhead: 30 s from the
  // current time, clamped to the recording's end when the duration is known.
  // The daemon anchors the moment on the conversation's link.
  function chatAboutMoment() {
    if (!recording || !media.current) return;
    const start = Math.round(media.current.currentTime * 1000);
    if (!Number.isSafeInteger(start) || start < 0) return;
    const end = Math.min(start + 30000, Number.isFinite(media.current.duration) ? Math.round(media.current.duration * 1000) : Infinity);
    // An empty range (playhead at the very end) would be refused on send: open the chat without it.
    ctx.openChat({ node: nodeId, purpose: 'meeting', ...(end > start ? { context: [{ kind: 'range', node: nodeId,
      attachment: recording.id, revision: recording.revision, start_ms: start, end_ms: end }] } : {}) });
  }
  return h('section', { className: 'meetings recording', 'aria-label': 'Recording' },
    h('h2', null, 'Recording'),
    assets.error ? h('p', { role: 'alert', className: 'error' }, String(assets.error), h(Button, { variant: 'link', onClick: () => void assets.refresh() }, 'Retry')) : null,
    !recordings.length && !assets.loading ? h('p', { className: 'muted' }, 'No recording yet. Upload audio or video to link moments to tasks.') : null,
    recordings.length > 0 ? h('label', null, 'Recording', h('select', { value: recording?.id || selected, onChange: (e) => { position.current = 0; setSelected(e.target.value); } },
      recordings.map((a) => h('option', { key: a.id, value: a.id }, a.name)))) : null,
    selected && !recording && !assets.loading ? h('p', { role: 'alert' }, 'The linked recording is unavailable.') : null,
    url && recording ? h(recording.media_type.startsWith('video/') ? 'video' : 'audio', {
      ref: media, src: url, controls: true, preload: 'metadata', 'aria-label': recording.name,
      onLoadedMetadata: loaded, onTimeUpdate: () => { position.current = media.current?.currentTime || 0; },
      onError: () => setMediaError('Playback failed. Check your connection and sign-in, or convert unsupported formats to a browser-playable format.'),
    }) : null,
    mediaError ? h('p', { role: 'alert', className: 'error' }, mediaError) : null,
    url && recording && chat ? h('div', { className: 'chat-row' },
      h(Button, { type: 'button', variant: 'outline', onClick: chatAboutMoment }, 'Chat about this moment'),
      h(Button, { type: 'button', variant: 'outline', onClick: () => ctx.openChat({ node: nodeId, purpose: 'meeting',
        context: [{ kind: 'attachment', node: nodeId, id: recording.id, revision: recording.revision }] }) }, 'Chat about this recording')) : null,
    editable ? h('div', { className: 'meetings' },
      h('label', { htmlFor: fieldId }, 'Upload recording (up to 8 GiB)', h(Input, { id: fieldId, type: 'file', accept: 'audio/wav,audio/mpeg,audio/ogg,audio/mp4,audio/webm,video/mp4,video/webm,video/ogg', disabled: busy,
        onChange: (e) => { setFile(e.target.files?.[0] || null); setOffset(0); setError(''); } })),
      file ? h('div', { className: 'upload-progress' }, h('progress', { value: offset, max: file.size, 'aria-label': 'Upload progress' }),
        h('span', { role: 'status' }, `${Math.floor(offset / file.size * 100)}%`),
        h(Button, { type: 'button', disabled: busy, onClick: () => void upload() }, busy ? 'Uploading…' : 'Upload / resume'),
        busy ? h(Button, { type: 'button', variant: 'outline', onClick: () => { pause.current = true; } }, 'Pause after chunk') : null) : null) : null,
    recording && writable && readLinks && can(ctx.projectId, 'links.write') ? h('form', { className: 'meetings', onSubmit: linkTime },
      h('label', null, 'Task to link at the current playback time', h('select', { value: target, onChange: (e) => setTarget(e.target.value), disabled: linking },
        h('option', { value: '' }, 'Choose a task'), (tasks.data || []).map((task) => h('option', { key: task.id, value: task.id }, task.title || task.id)))),
      tasks.error ? h('p', { role: 'alert' }, String(tasks.error)) : null,
      h(Button, { type: 'submit', disabled: !target || !url || linking }, linking ? 'Linking…' : 'Link current time')) : null,
    links.error ? h('p', { role: 'alert' }, String(links.error)) : null,
    h('ul', null, (links.data || []).map((link) => h('li', { key: link.id },
      h(Button, { variant: 'link', onClick: () => onOpenNode(link.target) }, tasks.data?.find((task) => task.id === link.target)?.title || link.target),
      link.anchors.map((anchor, i) => h(Button, { key: i, variant: 'outline', size: 'sm', onClick: () => {
        position.current = anchor.start_ms / 1000;
        if (recording?.id === anchor.attachment && recording.revision === anchor.revision && media.current) media.current.currentTime = position.current;
        else setSelected(anchor.attachment);
      } }, mediaTime(anchor.start_ms)))))),
    error ? h('p', { role: 'alert', className: 'error' }, error) : null,
    h('p', { role: 'status', className: 'muted' }, message));
}
