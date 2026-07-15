import { describe, it, expect } from 'vitest';
import {
  applyWorkerTabUpdate,
  clampDockHeight,
  dockHeightFromPointer,
  DEFAULT_DOCK_HEIGHT,
  MAX_DOCK_HEIGHT,
  MIN_DOCK_HEIGHT,
} from '../runDockUtils';

describe('applyWorkerTabUpdate — openRun tab routing', () => {
  it('adds a new tab keyed by runId when the run is not already open', () => {
    const next = applyWorkerTabUpdate([], 'run-abc', null);
    expect(next).toHaveLength(1);
    expect(next[0]).toMatchObject({ tabId: 'run-abc', runId: 'run-abc', draftPrompt: null });
  });

  it('does not duplicate tabs — re-opening an existing run updates the draft only', () => {
    const prev = [{ tabId: 'run-abc', runId: 'run-abc', draftPrompt: null }];
    const next = applyWorkerTabUpdate(prev, 'run-abc', 'new draft');
    expect(next).toHaveLength(1);
    expect(next[0].draftPrompt).toBe('new draft');
  });

  it('preserves an existing draft when the new call passes null', () => {
    const prev = [{ tabId: 'run-abc', runId: 'run-abc', draftPrompt: 'keep me' }];
    const next = applyWorkerTabUpdate(prev, 'run-abc', null);
    expect(next[0].draftPrompt).toBe('keep me');
  });

  it('leaves sibling tabs untouched when opening another run', () => {
    const prev = [{ tabId: 'run-abc', runId: 'run-abc', draftPrompt: 'mine' }];
    const next = applyWorkerTabUpdate(prev, 'run-xyz', null);
    expect(next).toHaveLength(2);
    expect(next[0]).toEqual(prev[0]);
  });

  // Height is dock-wide state (one remembered value), never per-tab: raising any
  // run reuses the height the user last dragged to.
  it('does not embed height into the tab record', () => {
    const next = applyWorkerTabUpdate([], 'run-abc', null);
    expect(next[0]).not.toHaveProperty('height');
  });
});

describe('clampDockHeight', () => {
  it('keeps a dragged height inside the allowed band', () => {
    expect(clampDockHeight(0.5)).toBe(0.5);
  });

  it('snaps past-the-top drags to full screen rather than overshooting', () => {
    expect(clampDockHeight(1.4)).toBe(MAX_DOCK_HEIGHT);
  });

  it('floors a too-small height at the minimum', () => {
    expect(clampDockHeight(0.01)).toBe(MIN_DOCK_HEIGHT);
  });

  it('falls back to full screen for unusable values', () => {
    expect(clampDockHeight(Number.NaN)).toBe(DEFAULT_DOCK_HEIGHT);
  });
});

describe('dockHeightFromPointer — top-border drag', () => {
  it('maps the pointer to the fraction of viewport below it', () => {
    expect(dockHeightFromPointer(400, 1000)).toEqual({ collapse: false, height: 0.6 });
  });

  it('reads a drag to the top of the screen as full screen', () => {
    expect(dockHeightFromPointer(0, 1000)).toEqual({ collapse: false, height: 1 });
  });

  it('collapses once the drag crosses below the bottom threshold', () => {
    expect(dockHeightFromPointer(950, 1000)).toEqual({ collapse: true });
  });

  it('collapses rather than clamping when dragged past the viewport bottom', () => {
    expect(dockHeightFromPointer(1200, 1000)).toEqual({ collapse: true });
  });

  it('never divides by a zero viewport', () => {
    expect(dockHeightFromPointer(0, 0)).toEqual({
      collapse: false,
      height: DEFAULT_DOCK_HEIGHT,
    });
  });
});
