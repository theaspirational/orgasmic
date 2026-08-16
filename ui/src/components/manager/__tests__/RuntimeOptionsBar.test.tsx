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
  vi.stubGlobal(
    'ResizeObserver',
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
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
  vi.unstubAllGlobals();
});

describe('RuntimeOptionsBar', () => {
  it('applies a model immediately when the live catalog supports switching', async () => {
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

    const picker = await screen.findByRole('button', { name: 'Choose provider and model' });
    expect(picker).toBeEnabled();
    fireEvent.click(picker);
    fireEvent.click(await screen.findByText('Fixture B'));
    await waitFor(() => {
      expect(postRunRuntimeOptionsMock).toHaveBeenCalledWith('run-live', {
        model: 'fixture-b',
      });
    });
  });

  it('does not fake a reset when the current live provider is clicked', async () => {
    fetchRunRuntimeOptionsMock.mockResolvedValue({
      run_id: 'run-live',
      catalog: liveCatalog(),
    });

    render(<RuntimeOptionsBar runId="run-live" />);

    fireEvent.click(await screen.findByRole('button', { name: 'Choose provider and model' }));
    const currentProvider = await screen.findByRole('button', { name: 'Codex' });
    expect(currentProvider).toBeDisabled();
    fireEvent.click(currentProvider);

    expect(postRunRuntimeOptionsMock).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Choose provider and model' })).toHaveTextContent(
      'Fixture A',
    );
  });

  it('shows fixed-session messaging when catalog fetch fails', async () => {
    fetchRunRuntimeOptionsMock.mockRejectedValue(new Error('capability_unsupported'));

    render(<RuntimeOptionsBar runId="run-unsupported" />);

    expect(await screen.findByText(/Codex options are fixed/)).toBeInTheDocument();
  });

  it('disables live controls when catalog lacks live_switching', async () => {
    fetchRunRuntimeOptionsMock.mockResolvedValue({
      run_id: 'run-static',
      catalog: { ...liveCatalog(), live_switching: false },
    });

    render(<RuntimeOptionsBar runId="run-static" />);

    expect(await screen.findByRole('button', { name: 'Choose provider and model' })).toBeDisabled();
  });
});
