// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
const { navigate, get, openChat } = vi.hoisted(() => ({ navigate: vi.fn(), get: vi.fn(), openChat: vi.fn() }));
vi.mock('@tanstack/react-router', () => ({ useNavigate: () => navigate, useRouterState: () => '/projects/demo/tasks' }));
vi.mock('@/hooks/useEventStream', () => ({ useEventStream: () => {} }));
vi.mock('@/lib/transport', () => ({ get }));
vi.mock('@/lib/runDock', () => ({ useOptionalRunDock: () => ({ openChat }) }));
import { NodeBacklinks } from '../NodeBacklinks';
afterEach(() => { cleanup(); vi.clearAllMocks(); });
it('loads target backlinks and opens the immutable source timestamp', async () => {
  get.mockResolvedValue([{ id: 'link', source: 'MEET-1', target: 'TASK-1', anchors: [{ attachment: 'asset', revision: 'sha', start_ms: 61000, label: 'Decision' }] }]);
  render(<NodeBacklinks projectId="demo" nodeId="TASK-1" />);
  fireEvent.click(await screen.findByRole('button', { name: '1:01 · Decision' }));
  expect(get).toHaveBeenCalledWith('/links?project=demo&node=TASK-1&incoming=true');
  expect(navigate.mock.calls[0][0].search({ drawer_stack: ['TASK-1'] })).toMatchObject({ drawer_stack: ['TASK-1', 'MEET-1'], media_node: 'MEET-1', media_attachment: 'asset', media_revision: 'sha', media_ms: 61000 });
});
it('groups conversations with their purpose and state and opens them in the dock', async () => {
  get.mockImplementation(async (path: string) => {
    if (path.startsWith('/links')) return [
      { id: 'l1', source: 'CONV-1', target: 'TASK-1', kind: 'RELATES_TO', deleted: false, anchors: [] },
      { id: 'l2', source: 'MEET-1', target: 'TASK-1', kind: 'RELATES_TO', deleted: false, anchors: [] },
    ];
    if (path === '/org/node?project=demo&id=CONV-1') return {
      id: 'CONV-1', kind: 'conversations', title: 'Regenerate the brief', todo: 'OPEN', tags: [], body: '', sections: [],
      properties: [{ key: 'PURPOSE', value: 'regenerate' }], source: { file: '', base_version: 'v1' },
    };
    throw new Error(`unexpected ${path}`);
  });
  render(<NodeBacklinks projectId="demo" nodeId="TASK-1" />);
  fireEvent.click(await screen.findByRole('button', { name: 'Regenerate the brief' }));
  expect(openChat).toHaveBeenCalledWith({ conversationId: 'CONV-1' });
  expect(screen.getByRole('heading', { name: 'Conversations' })).toBeInTheDocument();
  expect(screen.getByText('regenerate')).toBeInTheDocument();
  expect(screen.getByText('OPEN')).toBeInTheDocument();
  expect(screen.getByRole('button', { name: 'MEET-1' })).toBeInTheDocument();
  expect(navigate).not.toHaveBeenCalled();
});
