// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import { restorableStoredTabs } from '../runDock';
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
});
