// Minimal shape used by applyWorkerTabUpdate — mirrors WorkerTab in runDock.tsx.
// Kept here to avoid pulling in the browser-side api/transport chain in tests.
type WorkerTab = { tabId: string; runId: string | null; draftPrompt?: string | null };

// Pure tab-state updater for worker runs in openRun. Exported separately so
// unit tests can import it without pulling in the browser-side api/transport
// chain. // orgasmic:task_CJWT3.1
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
