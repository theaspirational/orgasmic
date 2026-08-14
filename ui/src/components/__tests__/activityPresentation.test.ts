// @vitest-environment jsdom

import { describe, expect, it } from 'vitest';

import {
  activityDayKey,
  activitySummary,
  humanizeActivityType,
} from '@/components/ActivityView';
import type { TxRecord } from '@/lib/types';

function record(overrides: Partial<TxRecord['entry']> = {}): TxRecord {
  return {
    source_path: '.orgasmic/tx.org',
    entry: {
      tx_id: 'tx-1',
      ty: 'manager.dispatch_started',
      time: '[2026-08-13 Thu 00:13:01]',
      actor: 'aspirational',
      machine: 'local',
      extra: [],
      ...overrides,
    },
  };
}

describe('Activity presentation', () => {
  it('groups a locally parsed near-midnight event under its local calendar date', () => {
    expect(activityDayKey(record())).toBe('2026-08-13');
  });

  it('turns known and unknown event identifiers into human labels', () => {
    expect(humanizeActivityType('task.state_transitioned')).toBe('Task status changed');
    expect(humanizeActivityType('custom_worker.started')).toBe('Custom worker started');
  });

  it('turns common machine-generated reasons into readable summaries', () => {
    expect(
      activitySummary(record({
        ty: 'task.state_transitioned',
        reason: 'transition TASK-P9T4N to in_review',
      }).entry),
    ).toBe('TASK-P9T4N moved to In review');

    expect(activitySummary(record({ reason: 'manager.dispatch_started' }).entry)).toBe(
      'Dispatch started',
    );
  });
});
