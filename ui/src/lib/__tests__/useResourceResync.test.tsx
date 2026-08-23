// @vitest-environment jsdom
import { act, render } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

const { subscribeResyncMock } = vi.hoisted(() => ({
  subscribeResyncMock: vi.fn(),
}));

vi.mock('@/lib/resync', () => ({
  subscribeResync: subscribeResyncMock,
}));

import { useResource } from '../useResource';

function Probe({ fetcher }: { fetcher: () => Promise<number> }) {
  useResource('probe', fetcher);
  return null;
}

describe('useResource staleness guards', () => {
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
