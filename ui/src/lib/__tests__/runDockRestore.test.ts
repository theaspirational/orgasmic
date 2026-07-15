// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { createElement, useEffect } from 'react';
import { afterEach, describe, expect, it } from 'vitest';

import { restorableStoredTabs, RunDockProvider, useRunDock } from '../runDock';
import type { RunSummary } from '../types';

function liveRun(runId: string, driver: string): RunSummary {
  return {
    run_id: runId,
    task_id: driver === 'external' ? 'manager.launch:proj' : 'TASK-ONE',
    kind: 'worker',
    role: 'implementer',
    driver,
    harness: driver === 'external' ? 'external' : 'claude',
    project_id: 'proj',
    sub_state: null,
    identity: { run_id: runId, runtime_id: `rt-${runId}`, boot_id: 'boot' },
    session_path: `/sessions/${runId}.jsonl`,
    event_count: 0,
  };
}

function OpenRunProbe({ runs, runId }: { runs: RunSummary[]; runId: string }) {
  const { tabs, openRun, replaceLiveRuns } = useRunDock();
  useEffect(() => replaceLiveRuns(runs), [replaceLiveRuns, runs]);
  return createElement(
    'div',
    null,
    createElement('button', { onClick: () => openRun({ runId }) }, 'Open run'),
    createElement('output', { 'aria-label': 'open tab count' }, tabs.length),
  );
}

// Unlike OpenRunProbe, synchronization here is a click, not an effect — the
// race under test is openRun landing BEFORE the live-run map knows the run.
function LateSyncProbe({ runs, runId }: { runs: RunSummary[]; runId: string }) {
  const { tabs, openRun, replaceLiveRuns } = useRunDock();
  return createElement(
    'div',
    null,
    createElement('button', { onClick: () => openRun({ runId }) }, 'Open run'),
    createElement('button', { onClick: () => replaceLiveRuns(runs) }, 'Sync runs'),
    createElement('output', { 'aria-label': 'open tab count' }, tabs.length),
  );
}

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});

describe('persisted Run Dock restore eligibility', () => {
  it('purges a saved external tab while restoring normal live and recovered tabs', () => {
    const stored = [
      { tabId: 'run-external', runId: 'run-external' },
      { tabId: 'run-worker', runId: 'run-worker' },
      { tabId: 'run-recovered', runId: 'run-recovered' },
    ];
    expect(
      restorableStoredTabs(
        stored,
        [liveRun('run-external', 'external'), liveRun('run-worker', 'rmux')],
        ['run-recovered'],
      ),
    ).toEqual([stored[1], stored[2]]);
  });

  it('rejects openRun without a driver argument when the live run is external', () => {
    const external = liveRun('run-external', 'external');
    render(
      createElement(
        RunDockProvider,
        null,
        createElement(OpenRunProbe, { runs: [external], runId: external.run_id }),
      ),
    );

    fireEvent.click(screen.getByRole('button', { name: 'Open run' }));
    expect(screen.getByLabelText('open tab count').textContent).toBe('0');
  });

  it('purges a tab that won the race against live-run synchronization', () => {
    const external = liveRun('run-external', 'external');
    render(
      createElement(
        RunDockProvider,
        null,
        createElement(LateSyncProbe, { runs: [external], runId: external.run_id }),
      ),
    );

    // The map is empty, so openRun admits the run like any stale tab...
    fireEvent.click(screen.getByRole('button', { name: 'Open run' }));
    expect(screen.getByLabelText('open tab count').textContent).toBe('1');
    // ...and the next synchronization learns it is live external and purges it.
    fireEvent.click(screen.getByRole('button', { name: 'Sync runs' }));
    expect(screen.getByLabelText('open tab count').textContent).toBe('0');
  });

  it('still opens an unknown or ended run so stale tabs remain usable', () => {
    render(
      createElement(
        RunDockProvider,
        null,
        createElement(OpenRunProbe, { runs: [], runId: 'run-ended' }),
      ),
    );

    fireEvent.click(screen.getByRole('button', { name: 'Open run' }));
    expect(screen.getByLabelText('open tab count').textContent).toBe('1');
  });
});
