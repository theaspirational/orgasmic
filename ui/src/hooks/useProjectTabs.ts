import { useEffect, useSyncExternalStore } from 'react';
import { useRouterState } from '@tanstack/react-router';

import {
  closeOthers,
  closeTab,
  closeToRight,
  getSnapshot,
  openTab,
  parseView,
  pruneTabs,
  reorderTabs,
  subscribe,
  type ProjectTab,
  type TabsState,
} from '@/lib/tabsStore';

export type UseProjectTabs = TabsState & {
  openTab: typeof openTab;
  closeTab: typeof closeTab;
  closeOthers: typeof closeOthers;
  closeToRight: typeof closeToRight;
  reorderTabs: typeof reorderTabs;
  pruneTabs: typeof pruneTabs;
};

export function useProjectTabs(): UseProjectTabs {
  const state = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  return {
    tabs: state.tabs,
    lastView: state.lastView,
    openTab,
    closeTab,
    closeOthers,
    closeToRight,
    reorderTabs,
    pruneTabs,
  };
}

function projectFromPath(pathname: string): { projectId: string; view: ProjectTab['view'] } | null {
  const parts = pathname.split('/').filter(Boolean).map(decodeURIComponent);
  if (parts[0] !== 'projects' || !parts[1]) return null;
  // Bare `/projects/$id` renders the decisions index; otherwise parts[2] is the view.
  const view = parts[2] ? parseView(parts[2]) ?? 'decisions' : 'decisions';
  return { projectId: parts[1], view };
}

/**
 * Keep the tab store in sync with the URL: whenever the route points at a
 * project, ensure that project has a tab and that the tab remembers the current
 * view. Covers deep links, the index auto-pick, and notification navigations —
 * not just clicks on the tab strip. Mount once (in the app shell).
 */
export function useTabSync(): void {
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  useEffect(() => {
    const match = projectFromPath(pathname);
    if (match) openTab(match.projectId, match.view);
  }, [pathname]);
}
