// @vitest-environment jsdom
import React from 'react';
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
vi.mock('@orgasmic/plugin-sdk', async () => ({
  ...(await import('@/lib/nodeServices')),
  useResource: (await import('@/lib/useResource')).useResource,
  useMe: () => ({ can: () => true }), useEventStream: () => {},
  Button: ({ variant, size, ...props }) => <button {...props} />,
  Input: (props) => <input {...props} />,
  Textarea: (props) => <textarea {...props} />,
}));
import { register } from '../../../../examples/plugins/meetings/ui/index.js';
import { Player } from '../../../../examples/plugins/meetings/ui/player.js';
afterEach(cleanup);

it('creates a meeting from pasted or imported notes without a CLI', async () => {
  let View;
  const onOpenNode = vi.fn();
  const ctx = {
    projectId: 'demo', signal: new AbortController().signal,
    registerStyles: vi.fn(), registerNodeView: vi.fn((_collection, view) => { View = view; return () => {}; }),
    get: vi.fn().mockResolvedValue([]),
    post: vi.fn().mockResolvedValue({ id: 'MEET-1' }),
  };
  register(ctx);
  render(<View projectId="demo" collection="meetings" onOpenNode={onOpenNode} />);

  expect(await screen.findByRole('button', { name: 'New meeting' })).toBeInTheDocument();
  expect(screen.queryByText(/orgasmic plugin run/)).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole('button', { name: 'New meeting' }));
  const notes = new File(['Discuss ORSL rollout.'], 'Planning with Max.txt', { type: 'text/plain' });
  notes.text = vi.fn().mockResolvedValue('Discuss ORSL rollout.');
  fireEvent.change(screen.getByLabelText('Import notes from a text file'), { target: { files: [notes] } });
  await waitFor(() => expect(screen.getByLabelText('Notes')).toHaveValue('Discuss ORSL rollout.'));
  expect(screen.getByLabelText('Title')).toHaveValue('Planning with Max');
  fireEvent.click(screen.getByRole('button', { name: 'Create meeting' }));

  await waitFor(() => expect(ctx.post).toHaveBeenCalledWith('/org/node', expect.objectContaining({
    kind: 'meetings', title: 'Planning with Max', body: 'Discuss ORSL rollout.', request_id: expect.any(String),
  })));
  await waitFor(() => expect(onOpenNode).toHaveBeenCalledWith('MEET-1'));
});

it('loads the task graph layer and writes the current immutable recording timestamp', async () => {
  const recording = { id: 'recording', name: 'Planning.wav', media_type: 'audio/wav', revision: 'sha' };
  const ctx = { projectId: 'demo', signal: new AbortController().signal, mediaUrl: vi.fn().mockReturnValue('/media'),
    get: vi.fn(async (path) => {
      if (path.startsWith('/attachments?')) return [recording];
      if (path === '/graph/nodes?layer=task') return [{ id: 'TASK-1', title: 'Follow up' }];
      return [];
    }), post: vi.fn().mockResolvedValue({}) };
  render(<Player ctx={ctx} nodeId="MEET-1" onOpenNode={vi.fn()} writable />);
  await screen.findByRole('option', { name: 'Follow up' });
  const media = await screen.findByLabelText('Planning.wav');
  Object.defineProperty(media, 'duration', { value: 120 }); media.currentTime = 90;
  fireEvent.change(screen.getByRole('combobox', { name: 'Task to link at the current playback time' }), { target: { value: 'TASK-1' } });
  fireEvent.click(screen.getByRole('button', { name: 'Link current time' }));
  await waitFor(() => expect(ctx.post).toHaveBeenCalledWith('/links', expect.objectContaining({ source: 'MEET-1', target: 'TASK-1', base_revision: 0, anchors: [{ attachment: 'recording', revision: 'sha', start_ms: 90000, label: '' }] })));
  expect(await screen.findByText('Linked TASK-1 at 1:30')).toBeInTheDocument();
});

function recordingCtx(extra = {}) {
  const recording = { id: 'recording', name: 'Planning.wav', media_type: 'audio/wav', revision: 'sha' };
  return { projectId: 'demo', signal: new AbortController().signal, mediaUrl: vi.fn().mockReturnValue('/media'),
    get: vi.fn(async (path) => (path.startsWith('/attachments?') ? [recording] : [])), post: vi.fn(), ...extra };
}

it('opens the meeting chat with a 30 s range chip at the playhead, clamped to the recording', async () => {
  const ctx = recordingCtx({ openChat: vi.fn() });
  render(<Player ctx={ctx} nodeId="MEET-1" onOpenNode={vi.fn()} writable />);
  const media = await screen.findByLabelText('Planning.wav');
  Object.defineProperty(media, 'duration', { value: 120 }); media.currentTime = 100;
  fireEvent.click(screen.getByRole('button', { name: 'Chat about this moment' }));
  expect(ctx.openChat).toHaveBeenCalledWith({ node: 'MEET-1', purpose: 'meeting',
    context: [{ kind: 'range', node: 'MEET-1', attachment: 'recording', revision: 'sha', start_ms: 100000, end_ms: 120000 }] });
});

it('opens the meeting chat without a range chip when the playhead is at the end', async () => {
  const ctx = recordingCtx({ openChat: vi.fn() });
  render(<Player ctx={ctx} nodeId="MEET-1" onOpenNode={vi.fn()} writable />);
  const media = await screen.findByLabelText('Planning.wav');
  Object.defineProperty(media, 'duration', { value: 120 }); media.currentTime = 120;
  fireEvent.click(screen.getByRole('button', { name: 'Chat about this moment' }));
  expect(ctx.openChat).toHaveBeenCalledWith({ node: 'MEET-1', purpose: 'meeting' });
});

it('opens the meeting chat with the recording as an attachment chip', async () => {
  const ctx = recordingCtx({ openChat: vi.fn() });
  render(<Player ctx={ctx} nodeId="MEET-1" onOpenNode={vi.fn()} writable />);
  await screen.findByLabelText('Planning.wav');
  fireEvent.click(screen.getByRole('button', { name: 'Chat about this recording' }));
  expect(ctx.openChat).toHaveBeenCalledWith({ node: 'MEET-1', purpose: 'meeting',
    context: [{ kind: 'attachment', node: 'MEET-1', id: 'recording', revision: 'sha' }] });
});

it('hides the chat controls on a host without core.chat', async () => {
  render(<Player ctx={recordingCtx()} nodeId="MEET-1" onOpenNode={vi.fn()} writable />);
  await screen.findByLabelText('Planning.wav');
  expect(screen.queryByRole('button', { name: /^Chat about/ })).toBeNull();
});

function detailView(body, extra = {}) {
  let View;
  const ctx = { projectId: 'demo', signal: new AbortController().signal, mediaUrl: vi.fn(),
    registerStyles: vi.fn(), registerNodeView: vi.fn((_, view) => { View = view; return () => {}; }),
    get: vi.fn(async (path) => (path.startsWith('/org/node?') ? { id: 'MEET-1', title: 'Planning', body, schema_matches: true, source: { base_version: 'v1' } } : [])),
    post: vi.fn(), getDraft: () => undefined, setDraft: vi.fn(), clearDraft: vi.fn(), ...extra };
  register(ctx);
  return { ctx, View };
}

it('chats about the selected notes text, capped at 4 KiB on a character boundary', async () => {
  const { ctx, View } = detailView(`${'€'.repeat(2000)} tail`, { openChat: vi.fn() });
  render(<View nodeId="MEET-1" projectId="demo" onOpenNode={vi.fn()} />);
  const notes = await screen.findByLabelText('Notes');
  const button = screen.getByRole('button', { name: 'Chat about selection' });
  expect(button).toBeDisabled();
  notes.setSelectionRange(0, 3);
  fireEvent.select(notes);
  expect(button).toBeEnabled();
  fireEvent.click(button);
  expect(ctx.openChat).toHaveBeenLastCalledWith({ node: 'MEET-1', purpose: 'meeting', context: [{ kind: 'selection', text: '€€€' }] });

  notes.setSelectionRange(0, notes.value.length);
  fireEvent.select(notes);
  fireEvent.click(button);
  const { text } = ctx.openChat.mock.lastCall[0].context[0];
  expect(new TextEncoder().encode(text).length).toBeLessThanOrEqual(4096);
  expect(text.endsWith('…')).toBe(true);
  expect(text.startsWith('€€€')).toBe(true);
  expect(text.includes('�')).toBe(false);

  // Editing collapses the selection.
  fireEvent.change(notes, { target: { value: 'rewritten' } });
  expect(button).toBeDisabled();
  fireEvent.click(screen.getByRole('button', { name: 'Chat' }));
  expect(ctx.openChat).toHaveBeenLastCalledWith({ node: 'MEET-1', purpose: 'meeting' });
});

it('hides the detail chat controls on a host without core.chat', async () => {
  const { View } = detailView('notes');
  render(<View nodeId="MEET-1" projectId="demo" onOpenNode={vi.fn()} />);
  await screen.findByLabelText('Notes');
  expect(screen.queryByRole('button', { name: /Chat/ })).toBeNull();
});
