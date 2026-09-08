// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
const { navigate, get } = vi.hoisted(() => ({ navigate: vi.fn(), get: vi.fn() }));
vi.mock('@tanstack/react-router', () => ({ useNavigate: () => navigate, useRouterState: () => '/projects/demo/tasks' }));
vi.mock('@/hooks/useEventStream', () => ({ useEventStream: () => {} }));
vi.mock('@/lib/transport', () => ({ get }));
import { NodeBacklinks } from '../NodeBacklinks';
afterEach(cleanup);
it('loads target backlinks and opens the immutable source timestamp', async () => {
  get.mockResolvedValue([{ id: 'link', source: 'MEET-1', target: 'TASK-1', anchors: [{ attachment: 'asset', revision: 'sha', start_ms: 61000, label: 'Decision' }] }]);
  render(<NodeBacklinks projectId="demo" nodeId="TASK-1" />);
  fireEvent.click(await screen.findByRole('button', { name: '1:01 · Decision' }));
  expect(get).toHaveBeenCalledWith('/links?project=demo&node=TASK-1&incoming=true');
  expect(navigate.mock.calls[0][0].search({ drawer_stack: ['TASK-1'] })).toMatchObject({ drawer_stack: ['TASK-1', 'MEET-1'], media_node: 'MEET-1', media_attachment: 'asset', media_revision: 'sha', media_ms: 61000 });
});
