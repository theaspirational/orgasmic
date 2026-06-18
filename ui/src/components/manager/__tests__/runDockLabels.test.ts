import { describe, expect, it } from 'vitest';

import { workerRunTabLabel } from '../runDockLabels';
import type { RunSummary } from '@/lib/types';

function run(overrides: Partial<RunSummary> = {}): RunSummary {
  return {
    run_id: 'run-live',
    task_id: 'TASK-SV032',
    kind: 'worker',
    role: 'reviewer',
    driver: 'rmux',
    harness: 'custom',
    worker_id: 'reviewer-claude-rmux',
    project_id: 'orgasmic',
    sub_state: null,
    identity: {
      run_id: 'run-live',
      runtime_id: 'runtime-live',
      boot_id: 'boot-live',
    },
    session_path: '.orgasmic/tmp/sessions/run-live.jsonl',
    event_count: 3,
    ...overrides,
  };
}

describe('workerRunTabLabel', () => {
  it('uses the live run title while the run is still active', () => {
    expect(workerRunTabLabel('run-live', run(), {})).toBe('TASK-SV032 · custom');
  });

  it('keeps the cached readable label after the run leaves the live map', () => {
    expect(
      workerRunTabLabel('run-ended', null, {
        'run-ended': 'TASK-K5RG2 · Claude tmux',
      }),
    ).toBe('TASK-K5RG2 · Claude tmux');
  });

  it('falls back to the run id only when no live or cached label exists', () => {
    expect(workerRunTabLabel('run-ended', null, {})).toBe('run-ended');
  });
});
