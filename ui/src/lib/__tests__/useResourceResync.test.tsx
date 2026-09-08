// @vitest-environment jsdom
import { act, cleanup, render, renderHook } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { subscribeResyncMock } = vi.hoisted(() => ({
  subscribeResyncMock: vi.fn(),
}));

vi.mock('@/lib/resync', () => ({
  subscribeResync: subscribeResyncMock,
}));

import { useResource } from '../useResource';

afterEach(cleanup);

function Probe({ fetcher }: { fetcher: () => Promise<number> }) {
  useResource('probe', fetcher);
  return null;
}

describe('useResource staleness guards', () => {
  it.each(['unmount', 'disable', 'reopen'])('ignores a pending request after %s', async (change) => {
    let reject = (_error: Error) => {};
    const pending = new Promise<number>((_resolve, rejectPromise) => { reject = rejectPromise; });
    const fetcher = vi.fn().mockReturnValueOnce(pending).mockResolvedValue(2);
    const onError = vi.fn();
    subscribeResyncMock.mockImplementation(() => () => {});
    const view = renderHook(({ enabled, key }) => useResource(key, fetcher, { enabled, onError }), {
      initialProps: { enabled: true, key: 'probe' },
    });
    const refresh = view.result.current.refresh;
    if (change === 'unmount') view.unmount();
    else {
      view.rerender({ enabled: false, key: 'probe' });
      if (change === 'reopen') view.rerender({ enabled: true, key: 'probe' });
    }
    await act(async () => { reject(new Error('late failure')); });
    expect(onError).not.toHaveBeenCalled();
    if (change === 'unmount') {
      await refresh();
      expect(fetcher).toHaveBeenCalledTimes(1);
    } else {
      expect(view.result.current.error).toBeNull();
      expect(view.result.current.loading).toBe(false);
      expect(view.result.current.data).toBe(change === 'reopen' ? 2 : null);
    }
  });

  it('refetches on stream resync and on returning to the foreground', async () => {
    let resync = () => {};
    subscribeResyncMock.mockImplementation((listener: () => void) => {
      resync = listener;
      return () => {};
    });
    const fetcher = vi.fn(async () => 1);

    render(<Probe fetcher={fetcher} />);
    await act(async () => {});
    expect(fetcher).toHaveBeenCalledTimes(1);

    await act(async () => resync());
    expect(fetcher).toHaveBeenCalledTimes(2);

    Object.defineProperty(document, 'visibilityState', {
      value: 'visible',
      configurable: true,
    });
    await act(async () => {
      document.dispatchEvent(new Event('visibilitychange'));
    });
    expect(fetcher).toHaveBeenCalledTimes(3);
  });
});
