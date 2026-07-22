// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { RuntimeOptionsCatalog } from '@/lib/types';

const fetchRunRuntimeOptionsMock = vi.fn();
const postRunRuntimeOptionsMock = vi.fn();

vi.mock('sonner', () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

vi.mock('@/lib/api', () => ({
  fetchRunRuntimeOptions: (...args: unknown[]) => fetchRunRuntimeOptionsMock(...args),
  postRunRuntimeOptions: (...args: unknown[]) => postRunRuntimeOptionsMock(...args),
}));

import { RuntimeOptionsBar } from '../RuntimeOptionsBar';

function liveCatalog(): RuntimeOptionsCatalog {
  return {
    source: 'cursor-acp:session/new',
    provider_switching: false,
    live_switching: true,
    current: { model: 'fixture-a', reasoning_effort: null },
    providers: [],
    models: [
      { id: 'fixture-a', label: 'Fixture A', current: true, reasoning_efforts: [], speeds: [] },
      { id: 'fixture-b', label: 'Fixture B', current: false, reasoning_efforts: [], speeds: [] },
    ],
    efforts: [],
    speeds: [],
  };
}

beforeEach(() => {
  Object.defineProperty(window.HTMLElement.prototype, 'hasPointerCapture', {
    value: () => false,
    configurable: true,
  });
  Object.defineProperty(window.HTMLElement.prototype, 'setPointerCapture', {
    value: () => {},
    configurable: true,
  });
  Object.defineProperty(window.HTMLElement.prototype, 'releasePointerCapture', {
    value: () => {},
    configurable: true,
  });
  Object.defineProperty(window.HTMLElement.prototype, 'scrollIntoView', {
    value: () => {},
    configurable: true,
  });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe('RuntimeOptionsBar', () => {
  it('enables Apply when the catalog supports live switching', async () => {
    fetchRunRuntimeOptionsMock.mockResolvedValue({
      run_id: 'run-live',
      catalog: liveCatalog(),
    });
    postRunRuntimeOptionsMock.mockResolvedValue({
      run_id: 'run-live',
      accepted: true,
      message: null,
    });

    render(<RuntimeOptionsBar runId="run-live" />);

    const apply = await screen.findByRole('button', { name: 'Apply' });
    expect(apply).toBeEnabled();

    fireEvent.click(apply);
    await waitFor(() => {
      expect(postRunRuntimeOptionsMock).toHaveBeenCalledWith('run-live', {
        model: 'fixture-a',
        reasoning_effort: null,
      });
    });
  });

  it('shows unsupported messaging and disabled Apply when catalog fetch fails', async () => {
    fetchRunRuntimeOptionsMock.mockRejectedValue(new Error('capability_unsupported'));

    render(<RuntimeOptionsBar runId="run-unsupported" />);

    expect(await screen.findByText(/Live runtime switching is not available/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Apply' })).toBeDisabled();
  });

  it('disables live controls when catalog lacks live_switching', async () => {
    fetchRunRuntimeOptionsMock.mockResolvedValue({
      run_id: 'run-static',
      catalog: { ...liveCatalog(), live_switching: false },
    });

    render(<RuntimeOptionsBar runId="run-static" />);

    expect(await screen.findByText('Live switch unsupported')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Apply' })).toBeDisabled();
  });
});
