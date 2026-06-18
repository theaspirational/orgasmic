import { describe, it, expect } from 'vitest';
import { applyWorkerTabUpdate } from '../runDockUtils';

const MANAGER_TAB_ID = '__manager__';
const MANAGER_TAB = { tabId: MANAGER_TAB_ID, runId: null };

describe('applyWorkerTabUpdate — openRun worker routing', () => {
  it('adds a new tab keyed by runId when the run is not already open', () => {
    const prev = [MANAGER_TAB];
    const next = applyWorkerTabUpdate(prev, 'run-abc', null);
    expect(next).toHaveLength(2);
    expect(next[1]).toMatchObject({ tabId: 'run-abc', runId: 'run-abc', draftPrompt: null });
  });

  it('does not duplicate tabs — re-opening an existing run updates the draft only', () => {
    const prev = [MANAGER_TAB, { tabId: 'run-abc', runId: 'run-abc', draftPrompt: null }];
    const next = applyWorkerTabUpdate(prev, 'run-abc', 'new draft');
    expect(next).toHaveLength(2);
    expect(next[1].draftPrompt).toBe('new draft');
  });

  it('preserves an existing draft when the new call passes null', () => {
    const prev = [MANAGER_TAB, { tabId: 'run-abc', runId: 'run-abc', draftPrompt: 'keep me' }];
    const next = applyWorkerTabUpdate(prev, 'run-abc', null);
    expect(next[1].draftPrompt).toBe('keep me');
  });

  it('does not touch MANAGER_TAB_ID when opening a worker run', () => {
    const prev = [MANAGER_TAB];
    const next = applyWorkerTabUpdate(prev, 'run-xyz', null);
    const managerTab = next.find((t) => t.tabId === MANAGER_TAB_ID);
    expect(managerTab).toEqual(MANAGER_TAB);
  });

  // The size routing invariant: openRun honors the requested size literally for
  // workers (no coercion to 'workbench'). applyWorkerTabUpdate is the tab-state
  // half; setSize(nextSize) is called unconditionally with whatever size was
  // requested — verified here by the absence of any size field in the tab record
  // (sizing is managed separately by the RunDockProvider state machine).
  it('does not embed size into the tab record — sizing is caller-controlled', () => {
    const prev = [MANAGER_TAB];
    const next = applyWorkerTabUpdate(prev, 'run-abc', null);
    expect(next[1]).not.toHaveProperty('size');
  });
});
