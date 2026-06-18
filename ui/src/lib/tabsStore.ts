/**
 * Multi-project tab strip state.
 *
 * The active project + view is owned by the URL (`/projects/$id/$view`); this
 * store owns the *separate* list of open tabs (one per project) plus a memory of
 * each project's last view so reopening restores where you were. It is a tiny
 * dependency-free `useSyncExternalStore` source persisted to localStorage so the
 * session survives reloads (and stays in sync across windows via the `storage`
 * event).
 */

const STORAGE_KEY = 'orgasmic.openTabs';

/** Project-scoped views a tab can point at (mirrors the router's project routes). */
export type TabView =
  | 'decisions'
  | 'architecture'
  | 'tasks'
  | 'glossary'
  | 'project'
  | 'runs'
  | 'prompts'
  | 'org'
  | 'activity'
  | 'status'
  | 'settings';

/** Default landing view for a freshly opened project tab (matches the index route). */
export const DEFAULT_TAB_VIEW: TabView = 'tasks';

const VIEW_SET = new Set<TabView>([
  'decisions',
  'architecture',
  'tasks',
  'glossary',
  'project',
  'runs',
  'prompts',
  'org',
  'activity',
  'status',
  'settings',
]);

export function parseView(raw: unknown): TabView | null {
  return typeof raw === 'string' && VIEW_SET.has(raw as TabView) ? (raw as TabView) : null;
}

export type ProjectTab = {
  projectId: string;
  view: TabView;
};

export type TabsState = {
  tabs: ProjectTab[];
  lastView: Record<string, TabView>;
};

const EMPTY: TabsState = { tabs: [], lastView: {} };

function loadState(): TabsState {
  if (typeof window === 'undefined') return EMPTY;
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return EMPTY;
    const parsed = JSON.parse(raw) as Partial<TabsState>;

    const seen = new Set<string>();
    const tabs: ProjectTab[] = Array.isArray(parsed.tabs)
      ? parsed.tabs
          .filter((tab): tab is ProjectTab => !!tab && typeof tab.projectId === 'string')
          .filter((tab) => (seen.has(tab.projectId) ? false : (seen.add(tab.projectId), true)))
          .map((tab) => ({ projectId: tab.projectId, view: parseView(tab.view) ?? DEFAULT_TAB_VIEW }))
      : [];

    const lastView: Record<string, TabView> = {};
    if (parsed.lastView && typeof parsed.lastView === 'object') {
      for (const [projectId, view] of Object.entries(parsed.lastView)) {
        const parsedView = parseView(view);
        if (parsedView) lastView[projectId] = parsedView;
      }
    }

    return { tabs, lastView };
  } catch {
    return EMPTY;
  }
}

let state: TabsState = loadState();
const listeners = new Set<() => void>();

function persist() {
  if (typeof window === 'undefined') return;
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
  } catch {
    // Ignore quota / private-mode write failures; tabs simply won't persist.
  }
}

function setState(next: TabsState) {
  state = next;
  persist();
  for (const listener of listeners) listener();
}

export function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function getSnapshot(): TabsState {
  return state;
}

/**
 * Ensure `projectId` has a tab and point it at `view`. Creating a tab appends it;
 * reopening an existing project just focuses it (no duplicates) and updates its
 * view. No-ops when nothing would change so it is safe to call on every route
 * change. Returns the resolved view.
 */
export function openTab(projectId: string, view?: TabView): TabView {
  const existing = state.tabs.find((tab) => tab.projectId === projectId);
  const resolved = view ?? existing?.view ?? state.lastView[projectId] ?? DEFAULT_TAB_VIEW;

  if (existing) {
    if (existing.view === resolved && state.lastView[projectId] === resolved) return resolved;
    setState({
      tabs: state.tabs.map((tab) => (tab.projectId === projectId ? { projectId, view: resolved } : tab)),
      lastView: { ...state.lastView, [projectId]: resolved },
    });
    return resolved;
  }

  setState({
    tabs: [...state.tabs, { projectId, view: resolved }],
    lastView: { ...state.lastView, [projectId]: resolved },
  });
  return resolved;
}

/**
 * Close a tab. Keeps its `lastView` so reopening restores the view. Returns the
 * project that should become active if the closed tab was active: the right
 * neighbor, else the left, else `null` when no tabs remain. The caller decides
 * whether to navigate (only needed when closing the active tab).
 */
export function closeTab(projectId: string): string | null {
  const index = state.tabs.findIndex((tab) => tab.projectId === projectId);
  if (index === -1) return state.tabs[0]?.projectId ?? null;

  const neighbor =
    state.tabs[index + 1]?.projectId ?? state.tabs[index - 1]?.projectId ?? null;

  setState({
    tabs: state.tabs.filter((tab) => tab.projectId !== projectId),
    lastView: state.lastView,
  });

  return neighbor;
}

export function closeOthers(projectId: string): void {
  const keep = state.tabs.find((tab) => tab.projectId === projectId);
  if (!keep || state.tabs.length === 1) return;
  setState({ tabs: [keep], lastView: state.lastView });
}

export function closeToRight(projectId: string): void {
  const index = state.tabs.findIndex((tab) => tab.projectId === projectId);
  if (index === -1 || index === state.tabs.length - 1) return;
  setState({ tabs: state.tabs.slice(0, index + 1), lastView: state.lastView });
}

export function reorderTabs(from: number, to: number): void {
  const { length } = state.tabs;
  if (from === to || from < 0 || to < 0 || from >= length || to >= length) return;
  const tabs = state.tabs.slice();
  const [moved] = tabs.splice(from, 1);
  tabs.splice(to, 0, moved);
  setState({ tabs, lastView: state.lastView });
}

/** Drop tabs whose project no longer exists (reconcile against the live list). */
export function pruneTabs(knownIds: string[]): void {
  const known = new Set(knownIds);
  const tabs = state.tabs.filter((tab) => known.has(tab.projectId));
  if (tabs.length === state.tabs.length) return;
  setState({ tabs, lastView: state.lastView });
}
