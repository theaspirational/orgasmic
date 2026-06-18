import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';

import { fetchRecoveryStatus, fetchRuns } from '@/lib/api';
import type { ManagerSize, RunSummary } from '@/lib/types';
import { applyWorkerTabUpdate } from '@/lib/runDockUtils';

const OPEN_TABS_KEY = 'orgasmic.rundock.open-tabs.v1';
const SIZE_KEY = 'orgasmic.rundock.size.v1';

// The Manager tab is always present and is keyed by a sentinel, not a run id:
// the active manager run can change (relaunch/switch) while the tab stays put.
export const MANAGER_TAB_ID = '__manager__';

export type RunDockTab = {
  // tabId is MANAGER_TAB_ID for the special manager tab, otherwise the run id.
  tabId: string;
  runId: string | null;
  // A staged recovery prompt to pre-fill the composer with on open. Cleared once
  // the user sends (explicit send only — never auto-sent).
  draftPrompt?: string | null;
};

type OpenRunOptions = {
  runId: string;
  role?: 'manager' | 'worker';
  draftPrompt?: string | null;
  // The requested sizing is honored literally: 'peek' opens a small live-transcript
  // window above the dock, 'workbench'/'focus' expand. Defaults to workbench so a
  // freshly opened run is visible unless the caller asks for a peek explicitly.
  size?: ManagerSize;
};

type RunDockContextValue = {
  open: boolean;
  size: ManagerSize;
  tabs: RunDockTab[];
  activeTabId: string;
  setSize: (size: ManagerSize) => void;
  setActiveTab: (tabId: string) => void;
  openManager: (size?: ManagerSize) => void;
  openRun: (options: OpenRunOptions) => void;
  closeTab: (tabId: string) => void;
  consumeDraft: (tabId: string) => void;
};

const RunDockContext = createContext<RunDockContextValue | null>(null);

type StoredTab = { tabId: string; runId: string | null };

function readStoredTabs(): StoredTab[] {
  if (typeof window === 'undefined') return [];
  try {
    const parsed = JSON.parse(window.localStorage.getItem(OPEN_TABS_KEY) ?? '[]') as unknown;
    if (!Array.isArray(parsed)) return [];
    return parsed.flatMap((entry) => {
      if (!entry || typeof entry !== 'object') return [];
      const tabId = (entry as { tabId?: unknown }).tabId;
      const runId = (entry as { runId?: unknown }).runId;
      if (typeof tabId !== 'string') return [];
      return [{ tabId, runId: typeof runId === 'string' ? runId : null }];
    });
  } catch {
    return [];
  }
}

function writeStoredTabs(tabs: RunDockTab[]): void {
  if (typeof window === 'undefined') return;
  // Drafts are intentionally not persisted: a staged prompt is session-scoped and
  // re-derived from backend run state on the next recovery open.
  const stored: StoredTab[] = tabs
    .filter((tab) => tab.tabId !== MANAGER_TAB_ID)
    .map((tab) => ({ tabId: tab.tabId, runId: tab.runId }));
  window.localStorage.setItem(OPEN_TABS_KEY, JSON.stringify(stored));
}

function readStoredSize(): ManagerSize {
  if (typeof window === 'undefined') return 'peek';
  const raw = window.localStorage.getItem(SIZE_KEY);
  return raw === 'workbench' || raw === 'focus' ? raw : 'peek';
}

export function RunDockProvider({ children }: { children: ReactNode }) {
  const [size, setSizeState] = useState<ManagerSize>(() => readStoredSize());
  const [tabs, setTabs] = useState<RunDockTab[]>(() => [
    { tabId: MANAGER_TAB_ID, runId: null },
  ]);
  const [activeTabId, setActiveTabId] = useState<string>(MANAGER_TAB_ID);
  const validatedRef = useRef(false);

  // Open = anything larger than the peek bar. The manager tab is always present
  // so "open" purely reflects whether the dock is expanded.
  const open = size !== 'peek';

  const setSize = useCallback((next: ManagerSize) => {
    setSizeState(next);
    if (typeof window !== 'undefined') window.localStorage.setItem(SIZE_KEY, next);
  }, []);

  // On first mount, restore persisted worker tabs but validate every run id
  // against backend run + recovery state; drop ids the daemon no longer knows.
  useEffect(() => {
    if (validatedRef.current) return;
    validatedRef.current = true;
    const stored = readStoredTabs();
    if (stored.length === 0) return;
    let cancelled = false;
    void (async () => {
      try {
        const [runs, recovery] = await Promise.all([
          fetchRuns().catch(() => null),
          fetchRecoveryStatus().catch(() => null),
        ]);
        if (cancelled) return;
        const known = new Set<string>();
        for (const run of runs?.live ?? []) known.add(run.run_id);
        for (const list of [
          runs?.interrupted,
          runs?.reattached,
          runs?.ambiguous,
          recovery?.interrupted_runs,
          recovery?.reattached_runs,
        ]) {
          for (const run of list ?? []) known.add(run.run_id);
        }
        const restored = stored.filter((tab) => tab.runId && known.has(tab.runId));
        if (restored.length === 0) return;
        setTabs((prev) => {
          const seen = new Set(prev.map((tab) => tab.tabId));
          const additions = restored
            .filter((tab) => !seen.has(tab.tabId))
            .map((tab) => ({ tabId: tab.tabId, runId: tab.runId }));
          return additions.length ? [...prev, ...additions] : prev;
        });
      } catch {
        // Validation is best-effort; on failure we simply keep only Manager.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    writeStoredTabs(tabs);
  }, [tabs]);

  const setActiveTab = useCallback((tabId: string) => {
    setActiveTabId(tabId);
  }, []);

  const openManager = useCallback(
    (nextSize: ManagerSize = 'workbench') => {
      setActiveTabId(MANAGER_TAB_ID);
      setSize(nextSize);
    },
    [setSize],
  );

  const openRun = useCallback(
    ({ runId, role, draftPrompt, size: nextSize = 'workbench' }: OpenRunOptions) => {
      if (role === 'manager') {
        // Manager recovery routes to the special Manager tab, which always tracks
        // the live manager run itself.
        setTabs((prev) =>
          prev.map((tab) =>
            tab.tabId === MANAGER_TAB_ID ? { ...tab, draftPrompt: draftPrompt ?? null } : tab,
          ),
        );
        setActiveTabId(MANAGER_TAB_ID);
        setSize(nextSize);
        return;
      }
      setTabs((prev) => applyWorkerTabUpdate(prev, runId, draftPrompt));
      setActiveTabId(runId);
      setSize(nextSize);
    },
    [setSize],
  );

  const closeTab = useCallback((tabId: string) => {
    // The Manager tab is permanent; closing detaches UI only and never stops the
    // run, so we just drop the tab from the dock.
    if (tabId === MANAGER_TAB_ID) return;
    setTabs((prev) => {
      const next = prev.filter((tab) => tab.tabId !== tabId);
      return next.length ? next : prev;
    });
    setActiveTabId((current) => (current === tabId ? MANAGER_TAB_ID : current));
  }, []);

  const consumeDraft = useCallback((tabId: string) => {
    setTabs((prev) =>
      prev.map((tab) => (tab.tabId === tabId ? { ...tab, draftPrompt: null } : tab)),
    );
  }, []);

  const value = useMemo<RunDockContextValue>(
    () => ({
      open,
      size,
      tabs,
      activeTabId,
      setSize,
      setActiveTab,
      openManager,
      openRun,
      closeTab,
      consumeDraft,
    }),
    [
      open,
      size,
      tabs,
      activeTabId,
      setSize,
      setActiveTab,
      openManager,
      openRun,
      closeTab,
      consumeDraft,
    ],
  );

  return <RunDockContext.Provider value={value}>{children}</RunDockContext.Provider>;
}

export function useRunDock(): RunDockContextValue {
  const ctx = useContext(RunDockContext);
  if (!ctx) throw new Error('useRunDock must be used within a RunDockProvider');
  return ctx;
}

// A no-throw variant for components that may render outside the provider in
// tests/storybook; returns null instead of throwing.
export function useOptionalRunDock(): RunDockContextValue | null {
  return useContext(RunDockContext);
}

export type { RunSummary };
