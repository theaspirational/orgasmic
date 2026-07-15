import { describe, expect, it } from 'vitest';

import {
  agentRuns,
  isExternalManagerRun,
  isTerminalRun,
  orderRunsByLaunch,
  terminalRunLabel,
  workerButtonLabel,
  workerRunTabLabel,
} from '../runDockLabels';
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

describe('orderRunsByLaunch — stable, oldest-first taskbar order', () => {
  it('orders runs by launch stamp regardless of how the API returned them', () => {
    const ordered = orderRunsByLaunch([
      run({ run_id: 'run-20260715T100358-939a781048cc4d019d9d7032528c187b' }),
      run({ run_id: 'run-20260715T100349-7d3d8ad0b3f0440998b329816c0fca5b' }),
    ]);
    // The first-launched terminal must be the one labelled "Terminal 1".
    expect(ordered.map((r) => r.run_id.slice(4, 19))).toEqual([
      '20260715T100349',
      '20260715T100358',
    ]);
  });

  it('breaks a same-second tie stably instead of leaving order to the caller', () => {
    const ids = ['run-20260715T095431-ccc', 'run-20260715T095431-aaa'];
    const first = orderRunsByLaunch(ids.map((run_id) => run({ run_id })));
    // Re-sorting the already-sorted list must not reshuffle it, or a label like
    // "Terminal 2" would hop between sessions on every refresh.
    const second = orderRunsByLaunch(first);
    expect(first.map((r) => r.run_id)).toEqual(second.map((r) => r.run_id));
    expect(first[0].run_id).toBe('run-20260715T095431-aaa');
  });

  it('does not mutate the caller list', () => {
    const runs = [run({ run_id: 'run-b' }), run({ run_id: 'run-a' })];
    orderRunsByLaunch(runs);
    expect(runs[0].run_id).toBe('run-b');
  });
});

describe('isTerminalRun — taskbar Manager pin vs Terminal buttons', () => {
  it('flags a manager.launch run with the custom pseudo-harness as a terminal', () => {
    expect(
      isTerminalRun(run({ task_id: 'manager.launch:orgasmic', harness: 'custom' })),
    ).toBe(true);
  });

  it('keeps an agent manager (non-custom harness) on the Manager pin', () => {
    expect(
      isTerminalRun(run({ task_id: 'manager.launch:orgasmic', harness: 'claude' })),
    ).toBe(false);
  });

  it('treats a legacy manager run without a harness as an agent manager', () => {
    expect(
      isTerminalRun(run({ task_id: 'manager.launch:orgasmic', harness: null })),
    ).toBe(false);
  });

  it('never flags worker runs, even with the custom harness', () => {
    expect(isTerminalRun(run({ task_id: 'TASK-SV032', harness: 'custom' }))).toBe(false);
  });
});

describe('agentRuns — the Running Agents menu shows supervised agents only', () => {
  it('drops bare terminals but keeps workers and agent managers', () => {
    const worker = run({ task_id: 'TASK-SV032', harness: 'custom' });
    const manager = run({ task_id: 'manager.launch:orgasmic', harness: 'claude' });
    const terminal = run({ task_id: 'manager.launch:orgasmic#terminal-1', harness: 'custom' });
    expect(agentRuns([worker, manager, terminal])).toEqual([worker, manager]);
  });

  it('yields an empty list — and so a hidden count badge — when only terminals are live', () => {
    const terminal = run({ task_id: 'manager.launch:orgasmic', harness: 'custom' });
    expect(agentRuns([terminal])).toEqual([]);
  });

  it('keeps an external manager registration (dec_3Y2E1) — it is a supervised run, not a terminal', () => {
    const external = run({
      task_id: 'manager.launch:orgasmic',
      driver: 'external',
      harness: 'external',
    });
    expect(agentRuns([external])).toEqual([external]);
  });
});

describe('isExternalManagerRun — dec_3Y2E1 external manager self-registration', () => {
  it('flags a run with driver external', () => {
    expect(isExternalManagerRun(run({ driver: 'external' }))).toBe(true);
  });

  it('is case/whitespace tolerant', () => {
    expect(isExternalManagerRun(run({ driver: ' External ' }))).toBe(true);
  });

  it('does not flag ordinary drivers', () => {
    expect(isExternalManagerRun(run({ driver: 'tmux' }))).toBe(false);
    expect(isExternalManagerRun(run({ driver: null }))).toBe(false);
  });
});

describe('terminal and worker taskbar labels', () => {
  it('labels a single terminal without an index', () => {
    expect(terminalRunLabel(0, 1)).toBe('Terminal');
  });

  it('numbers terminals once more than one is live', () => {
    expect(terminalRunLabel(0, 2)).toBe('Terminal 1');
    expect(terminalRunLabel(1, 2)).toBe('Terminal 2');
  });

  it('uses the bare task id for worker buttons', () => {
    expect(workerButtonLabel(run())).toBe('TASK-SV032');
  });

  it('falls back to Run when the task id is blank', () => {
    expect(workerButtonLabel(run({ task_id: '  ' }))).toBe('Run');
  });
});
