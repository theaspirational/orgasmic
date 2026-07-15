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
import { isRunDockEligible } from '@/lib/runLabels';
import type { RunSummary } from '@/lib/types';
import { applyWorkerTabUpdate, clampDockHeight, DEFAULT_DOCK_HEIGHT } from '@/lib/runDockUtils';

const OPEN_TABS_KEY = 'orgasmic.rundock.open-tabs.v1';
const OPEN_KEY = 'orgasmic.rundock.open.v1';
const ACTIVE_TAB_KEY = 'orgasmic.rundock.active-tab.v1';
const HEIGHT_KEY = 'orgasmic.rundock.height.v1';

export type RunDockTab = {
  // Every tab — manager, terminal, worker — is keyed by its run id. The dock
  // has no special tab (dec_FBBT2's Manager tab was retired with the taskbar
  // redesign): the manager is a peer run like any other.
  tabId: string;
  runId: string;
  // A staged recovery prompt to pre-fill the composer with on open. Cleared once
  // the user sends (explicit send only — never auto-sent).
  draftPrompt?: string | null;
};

type OpenRunOptions = {
  runId: string;
  draftPrompt?: string | null;
  driver?: string | null;
};

type RunDockContextValue = {
  /** Open = the session panel is showing above the taskbar. */
  open: boolean;
  /** Panel height as a viewport fraction (0..1); the taskbar is excluded. */
  height: number;
  tabs: RunDockTab[];
  activeTabId: string | null;
  setHeight: (height: number) => void;
  setActiveTab: (tabId: string) => void;
  /** Raise a run: select it and show the panel at the remembered height. */
  openRun: (options: OpenRunOptions) => void;
  /** Replace the current live-run metadata used to guard dock eligibility. */
  replaceLiveRuns: (runs: RunSummary[]) => void;
  /** Collapse to the bare taskbar, keeping the active selection. */
  minimize: () => void;
  closeTab: (tabId: string) => void;
  consumeDraft: (tabId: string) => void;
};

const RunDockContext = createContext<RunDockContextValue | null>(null);

export type StoredTab = { tabId: string; runId: string };

export function restorableStoredTabs(
  stored: StoredTab[],
  liveRuns: RunSummary[],
  recoveredRunIds: Iterable<string>,
): StoredTab[] {
  const known = new Set(recoveredRunIds);
  for (const run of liveRuns) {
    if (isRunDockEligible(run)) known.add(run.run_id);
  }
  return stored.filter((tab) => known.has(tab.runId));
}

function readStoredTabs(): StoredTab[] {
  if (typeof window === 'undefined') return [];
  try {
    const parsed = JSON.parse(window.localStorage.getItem(OPEN_TABS_KEY) ?? '[]') as unknown;
    if (!Array.isArray(parsed)) return [];
    return parsed.flatMap((entry) => {
      if (!entry || typeof entry !== 'object') return [];
      const tabId = (entry as { tabId?: unknown }).tabId;
      const runId = (entry as { runId?: unknown }).runId;
      if (typeof tabId !== 'string' || typeof runId !== 'string') return [];
      return [{ tabId, runId }];
    });
  } catch {
    return [];
  }
}

function writeStoredTabs(tabs: RunDockTab[]): void {
  if (typeof window === 'undefined') return;
  // Drafts are intentionally not persisted: a staged prompt is session-scoped and
  // re-derived from backend run state on the next recovery open.
  const stored: StoredTab[] = tabs.map((tab) => ({ tabId: tab.tabId, runId: tab.runId }));
  window.localStorage.setItem(OPEN_TABS_KEY, JSON.stringify(stored));
}

function readStoredOpen(): boolean {
  if (typeof window === 'undefined') return false;
  return window.localStorage.getItem(OPEN_KEY) === '1';
}

function readStoredActiveTab(): string | null {
  if (typeof window === 'undefined') return null;
  return window.localStorage.getItem(ACTIVE_TAB_KEY);
}

function readStoredHeight(): number {
  if (typeof window === 'undefined') return DEFAULT_DOCK_HEIGHT;
  const raw = window.localStorage.getItem(HEIGHT_KEY);
  const parsed = raw === null ? Number.NaN : Number(raw);
  // An unresized dock opens full-screen; a dragged one reopens where the user
  // left it.
  return Number.isFinite(parsed) ? clampDockHeight(parsed) : DEFAULT_DOCK_HEIGHT;
}

export function RunDockProvider({ children }: { children: ReactNode }) {
  const [openState, setOpen] = useState<boolean>(() => readStoredOpen());
  const [height, setHeightState] = useState<number>(() => readStoredHeight());
  const [tabs, setTabs] = useState<RunDockTab[]>([]);
  const [activeTabId, setActiveTabIdState] = useState<string | null>(() =>
    readStoredActiveTab(),
  );
  const validatedRef = useRef(false);
  const liveRunsRef = useRef<Map<string, RunSummary>>(new Map());

  // A dock with nothing selected has nothing to show, so it stays down. This
  // also covers the reload window before persisted tabs finish validating.
  const open = openState && activeTabId !== null;

  const setActiveTabId = useCallback((tabId: string | null) => {
    setActiveTabIdState(tabId);
    if (typeof window === 'undefined') return;
    if (tabId === null) window.localStorage.removeItem(ACTIVE_TAB_KEY);
    else window.localStorage.setItem(ACTIVE_TAB_KEY, tabId);
  }, []);

  const setHeight = useCallback((next: number) => {
    const clamped = clampDockHeight(next);
    setHeightState(clamped);
    if (typeof window !== 'undefined')
      window.localStorage.setItem(HEIGHT_KEY, String(clamped));
  }, []);

  const minimize = useCallback(() => {
    setOpen(false);
    if (typeof window !== 'undefined') window.localStorage.setItem(OPEN_KEY, '0');
  }, []);

  const replaceLiveRuns = useCallback((runs: RunSummary[]) => {
    liveRunsRef.current = new Map(runs.map((run) => [run.run_id, run]));
    // openRun admits a run the map does not know yet (stale/ended tabs must
    // stay openable — dec_FBBT2), so a tab opened during the sync gap could be
    // an external run. Purge tabs only for ids now positively known to be live
    // and external; everything else is untouched.
    const externalIds = new Set(
      runs.filter((run) => !isRunDockEligible(run)).map((run) => run.run_id),
    );
    if (externalIds.size === 0) return;
    setTabs((prev) =>
      prev.some((tab) => externalIds.has(tab.runId))
        ? prev.filter((tab) => !externalIds.has(tab.runId))
        : prev,
    );
    setActiveTabIdState((current) => {
      if (!current || !externalIds.has(current)) return current;
      if (typeof window !== 'undefined') window.localStorage.removeItem(ACTIVE_TAB_KEY);
      return null;
    });
  }, []);

  // On first mount, restore persisted tabs but validate every run id against
  // backend run + recovery state; drop ids the daemon no longer knows.
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
        replaceLiveRuns(runs?.live ?? []);
        const recoveredRunIds: string[] = [];
        for (const list of [
          runs?.interrupted,
          runs?.reattached,
          runs?.ambiguous,
          recovery?.interrupted_runs,
          recovery?.reattached_runs,
        ]) {
          for (const run of list ?? []) recoveredRunIds.push(run.run_id);
        }
        const restored = restorableStoredTabs(stored, runs?.live ?? [], recoveredRunIds);
        // Purge rejected ids immediately, including external tabs persisted by
        // an older UI build, rather than waiting for a later state effect.
        writeStoredTabs(restored);
        // A persisted selection whose run died while the page was closed can no
        // longer be raised; drop it so the dock restores collapsed rather than
        // onto an empty panel.
        setActiveTabIdState((current) =>
          current && restored.some((tab) => tab.tabId === current) ? current : null,
        );
        if (restored.length === 0) return;
        setTabs((prev) => {
          const seen = new Set(prev.map((tab) => tab.tabId));
          const additions = restored.filter((tab) => !seen.has(tab.tabId));
          return additions.length ? [...prev, ...additions] : prev;
        });
      } catch {
        // Validation is best-effort; on failure the dock simply starts empty.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [replaceLiveRuns]);

  useEffect(() => {
    writeStoredTabs(tabs);
  }, [tabs]);

  const setActiveTab = useCallback(
    (tabId: string) => {
      setActiveTabId(tabId);
    },
    [setActiveTabId],
  );

  const openRun = useCallback(
    ({ runId, draftPrompt, driver }: OpenRunOptions) => {
      const liveRun = liveRunsRef.current.get(runId);
      if (!isRunDockEligible(liveRun ?? { driver })) return;
      setTabs((prev) => applyWorkerTabUpdate(prev, runId, draftPrompt));
      setActiveTabId(runId);
      setOpen(true);
      if (typeof window !== 'undefined') window.localStorage.setItem(OPEN_KEY, '1');
    },
    [setActiveTabId],
  );

  const closeTab = useCallback(
    (tabId: string) => {
      // Closing detaches the UI only; it never stops or releases the run
      // (stop/release stays a separate explicit action — dec_FBBT2).
      setTabs((prev) => prev.filter((tab) => tab.tabId !== tabId));
      // Closing the tab you were looking at puts the dock down: there is no
      // "next" tab worth guessing at, and the taskbar is one click away.
      setActiveTabIdState((current) => {
        if (current !== tabId) return current;
        if (typeof window !== 'undefined') window.localStorage.removeItem(ACTIVE_TAB_KEY);
        return null;
      });
    },
    [],
  );

  const consumeDraft = useCallback((tabId: string) => {
    setTabs((prev) =>
      prev.map((tab) => (tab.tabId === tabId ? { ...tab, draftPrompt: null } : tab)),
    );
  }, []);

  const value = useMemo<RunDockContextValue>(
    () => ({
      open,
      height,
      tabs,
      activeTabId,
      setHeight,
      setActiveTab,
      openRun,
      replaceLiveRuns,
      minimize,
      closeTab,
      consumeDraft,
    }),
    [
      open,
      height,
      tabs,
      activeTabId,
      setHeight,
      setActiveTab,
      openRun,
      replaceLiveRuns,
      minimize,
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
