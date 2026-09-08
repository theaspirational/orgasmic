// @vitest-environment jsdom
import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import type { DaemonEvent } from '../types';
import { notifyResync } from '../resync';
import { usePluginRuntime } from '../pluginRuntime';

const mocks = vi.hoisted(() => ({
  event: (_event: DaemonEvent) => {},
  request: vi.fn(), session: vi.fn(), toast: vi.fn(),
}));
vi.mock('@/hooks/useEventStream', () => ({ useEventStream: (callback: typeof mocks.event) => { mocks.event = callback; } }));
vi.mock('../transport', () => ({ requestWithProfile: mocks.request, ensurePluginUiSession: mocks.session }));
vi.mock('sonner', () => ({ toast: { error: mocks.toast } }));
afterEach(() => { cleanup(); vi.useRealTimers(); vi.clearAllMocks(); });

it('checks sessions only for wanted UI, reports origin errors once across mounts, and refreshes only on events/reconnect', async () => {
  vi.useFakeTimers();
  mocks.request.mockResolvedValue([]);
  mocks.session.mockRejectedValue(new Error('Open the app from the selected backend to load its same-origin plugin UI.'));
  const profile = { baseUrl: 'https://remote.example' };
  const hook = renderHook(({ project }) => usePluginRuntime(project, profile, true, 'admin'), { initialProps: { project: 'a' } });
  await act(async () => {});
  expect(mocks.request).toHaveBeenCalledTimes(1);
  expect(mocks.session).not.toHaveBeenCalled();
  expect(mocks.toast).not.toHaveBeenCalled();
  await act(async () => { hook.rerender({ project: 'b' }); });
  expect(mocks.request).toHaveBeenCalledTimes(2);
  await act(async () => { vi.advanceTimersByTime(60_000); });
  expect(mocks.request).toHaveBeenCalledTimes(2);
  mocks.request.mockResolvedValue([{ id: 'meetings', enabled: true, error: null, manifest: { ui: 'ui/index.js' }, revision: '1' }]);
  await act(async () => { mocks.event({ topic: 'board', payload: { kind: 'board_refreshed' } } as DaemonEvent); });
  expect(mocks.session).toHaveBeenCalledTimes(1);
  expect(mocks.toast).toHaveBeenCalledTimes(1);
  await act(async () => { hook.rerender({ project: 'c' }); });
  await act(async () => { notifyResync(); });
  expect(mocks.request).toHaveBeenCalledTimes(5);
  expect(mocks.toast).toHaveBeenCalledTimes(1);
  hook.unmount();
  await act(async () => { notifyResync(); });
  expect(mocks.request).toHaveBeenCalledTimes(5);
});
