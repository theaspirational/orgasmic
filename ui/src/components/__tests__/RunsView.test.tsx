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

import { RunsView } from '../RunsView';

function run(runId: string, driver: string): RunSummary {
  return {
    run_id: runId,
    task_id: driver === 'external' ? 'manager.launch:proj' : 'TASK-ONE',
    kind: 'worker',
    role: driver === 'external' ? 'manager' : 'implementer',
    driver,
    harness: driver === 'external' ? 'external' : 'claude',
    project_id: 'proj',
    sub_state: null,
    identity: { run_id: runId, runtime_id: `rt-${runId}`, boot_id: 'boot' },
    session_path: `/sessions/${runId}.jsonl`,
    event_count: 0,
  };
}

function response(live: RunSummary[]): RunsResponse {
  return { live, interrupted: [], reattached: [], ambiguous: [], terminal_noop: [] };
}

describe('RunsView external manager action', () => {
  beforeEach(() => {
    openRunMock.mockClear();
    postRunReleaseMock.mockClear();
    fetchRunsMock.mockReset();
  });

  afterEach(cleanup);

  it('renders End instead of Open and releases without creating a dock tab', async () => {
    fetchRunsMock.mockResolvedValue(response([run('run-external', 'external')]));
    render(<RunsView projectId="proj" />);

    const end = await screen.findByRole('button', { name: 'End' });
    expect(screen.queryByRole('button', { name: 'Open' })).not.toBeInTheDocument();
    fireEvent.click(end);

    await waitFor(() => expect(postRunReleaseMock).toHaveBeenCalledWith('run-external'));
    expect(openRunMock).not.toHaveBeenCalled();
  });

  it('keeps Open for an attachable run', async () => {
    fetchRunsMock.mockResolvedValue(response([run('run-worker', 'rmux')]));
    render(<RunsView projectId="proj" />);
    fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
    expect(openRunMock).toHaveBeenCalledWith({ runId: 'run-worker' });
  });
});
