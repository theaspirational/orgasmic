// Minimal shape used by applyWorkerTabUpdate — mirrors RunDockTab in runDock.tsx.
// Kept here to avoid pulling in the browser-side api/transport chain in tests.
type WorkerTab = { tabId: string; runId: string; draftPrompt?: string | null };

// Pure tab-state updater for openRun. Exported separately so unit tests can
// import it without pulling in the browser-side api/transport chain.
// // orgasmic:task_CJWT3.1
export function applyWorkerTabUpdate(
  prev: WorkerTab[],
  runId: string,
  draftPrompt?: string | null,
): WorkerTab[] {
  const existing = prev.find((tab) => tab.tabId === runId);
  if (existing) {
    return prev.map((tab) =>
      tab.tabId === runId
        ? { ...tab, draftPrompt: draftPrompt ?? tab.draftPrompt ?? null }
        : tab,
    );
  }
  return [...prev, { tabId: runId, runId, draftPrompt: draftPrompt ?? null }];
}

// Dock panel height as a viewport fraction. An unresized dock raises
// full-screen; dragging past either end snaps to the bounds (the taskbar
// itself is never part of this measurement).
export const DEFAULT_DOCK_HEIGHT = 1;
export const MIN_DOCK_HEIGHT = 0.2;
export const MAX_DOCK_HEIGHT = 1;
// Dragging the top border below the minimum reads as "put it away".
export const DOCK_COLLAPSE_HEIGHT = 0.12;

export function clampDockHeight(value: number): number {
  if (!Number.isFinite(value)) return DEFAULT_DOCK_HEIGHT;
  return Math.min(MAX_DOCK_HEIGHT, Math.max(MIN_DOCK_HEIGHT, value));
}

// Translate a pointer's viewport Y into the dock outcome: the height the panel
// should take, or a collapse once the drag crosses below the minimum.
export function dockHeightFromPointer(
  clientY: number,
  viewportHeight: number,
): { collapse: true } | { collapse: false; height: number } {
  if (viewportHeight <= 0) return { collapse: false, height: DEFAULT_DOCK_HEIGHT };
  const fraction = 1 - clientY / viewportHeight;
  if (fraction < DOCK_COLLAPSE_HEIGHT) return { collapse: true };
  return { collapse: false, height: clampDockHeight(fraction) };
}
