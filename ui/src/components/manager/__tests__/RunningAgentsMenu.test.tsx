// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { RunSummary, RunsResponse } from '@/lib/types';

const { openRunMock, postRunReleaseMock, fetchRunsMock } = vi.hoisted(() => ({
  openRunMock: vi.fn(),
  postRunReleaseMock: vi.fn(async () => ({})),
  fetchRunsMock: vi.fn(),
}));

vi.mock('@/hooks/useEventStream', () => ({
  useEventStream: vi.fn(),
}));

vi.mock('@/lib/runDock', () => ({
  useRunDock: () => ({ openRun: openRunMock }),
}));

vi.mock('@/lib/api', async () => {
  const actual = await vi.importActual<typeof import('@/lib/api')>('@/lib/api');
  return {
    ...actual,
    fetchRuns: fetchRunsMock,
    postRunRelease: postRunReleaseMock,
  };
});

import { RunningAgentsMenu } from '../RunningAgentsMenu';

// Radix's DropdownMenu dismissable layer touches pointer-capture and
// scroll APIs jsdom does not implement; stub them so the trigger actually
// opens content under simulated events.
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

function identity(runId: string) {
  return { run_id: runId, runtime_id: `rt-${runId}`, boot_id: 'boot-1' };
}

function externalRun(runId: string): RunSummary {
  return {
    run_id: runId,
    task_id: 'manager.launch:proj',
    kind: 'manager',
    role: 'manager',
    driver: 'external',
    harness: 'external',
    project_id: 'proj',
    sub_state: null,
    identity: identity(runId),
    session_path: `/sessions/${runId}.jsonl`,
    event_count: 0,
  };
}

function workerRun(runId: string): RunSummary {
  return {
    run_id: runId,
    task_id: 'TASK-XYZ',
    kind: 'implementer',
    role: 'implementer',
    driver: 'tmux',
    harness: 'claude',
    project_id: 'proj',
    sub_state: null,
    identity: identity(runId),
    session_path: `/sessions/${runId}.jsonl`,
    event_count: 3,
  };
}

function runsResponse(live: RunSummary[]): RunsResponse {
  return { live, interrupted: [], reattached: [], terminal_noop: [], ambiguous: [] };
}

async function openMenu() {
  // Radix's DropdownMenuTrigger opens on `pointerdown`, not `click`.
  fireEvent.pointerDown(screen.getByRole('button', { name: /running agents/i }), {
    button: 0,
    ctrlKey: false,
  });
  await waitFor(() => expect(fetchRunsMock).toHaveBeenCalled());
}

describe('RunningAgentsMenu external manager row', () => {
  beforeEach(() => {
    openRunMock.mockClear();
    postRunReleaseMock.mockClear();
    fetchRunsMock.mockReset();
  });

  afterEach(() => {
    cleanup();
  });

  it('keeps an external manager run in the list (agentRuns does not drop it)', async () => {
    fetchRunsMock.mockResolvedValue(runsResponse([externalRun('run-ext-1')]));
    render(<RunningAgentsMenu projectId="proj" />);
    await openMenu();
    expect(await screen.findByText(/Manager · External/i)).toBeInTheDocument();
  });

  it('renders an End control for the external row and releases without opening', async () => {
    fetchRunsMock.mockResolvedValue(runsResponse([externalRun('run-ext-2')]));
    render(<RunningAgentsMenu projectId="proj" />);
    await openMenu();

    const endButton = await screen.findByRole('button', {
      name: /end manager registration/i,
    });
    fireEvent.click(endButton);

    await waitFor(() => expect(postRunReleaseMock).toHaveBeenCalledWith('run-ext-2'));
    expect(openRunMock).not.toHaveBeenCalled();
  });

  it('clicking the external row itself does not call openRun', async () => {
    fetchRunsMock.mockResolvedValue(runsResponse([externalRun('run-ext-3')]));
    render(<RunningAgentsMenu projectId="proj" />);
    await openMenu();

    const row = await screen.findByText(/Manager · External/i);
    fireEvent.click(row);

    expect(openRunMock).not.toHaveBeenCalled();
  });

  it('a normal worker row still opens the run on click', async () => {
    fetchRunsMock.mockResolvedValue(runsResponse([workerRun('run-worker-1')]));
    render(<RunningAgentsMenu projectId="proj" />);
    await openMenu();

    const row = await screen.findByText(/TASK-XYZ/i);
    fireEvent.click(row);

    expect(openRunMock).toHaveBeenCalledWith({ runId: 'run-worker-1' });
  });
});
