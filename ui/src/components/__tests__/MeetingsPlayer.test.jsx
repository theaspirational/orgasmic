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
