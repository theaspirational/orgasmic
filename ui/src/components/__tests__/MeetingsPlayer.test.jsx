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
}));
import { Player } from '../../../../examples/plugins/meetings/ui/player.js';
afterEach(cleanup);
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

it('hides the chat control on a host without core.chat', async () => {
  render(<Player ctx={recordingCtx()} nodeId="MEET-1" onOpenNode={vi.fn()} writable />);
  await screen.findByLabelText('Planning.wav');
  expect(screen.queryByRole('button', { name: 'Chat about this moment' })).toBeNull();
});
