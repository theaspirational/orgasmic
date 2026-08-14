import { searchList, type AppSearch } from '@/lib/searchState';

export type EntityPeekSource = 'drawer_stack' | 'peek_task' | 'task';

export type EntityPeekState = {
  activeId: string;
  source: EntityPeekSource;
  stack: string[];
};

function optionalString(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  return trimmed || null;
}

function isTasksPath(pathname: string): boolean {
  return /^\/projects\/[^/]+\/tasks\/?$/.test(pathname);
}

export function isTaskNodeId(id: string | null | undefined): boolean {
  return Boolean(id && /^TASK-/.test(id));
}

export function getEntityPeek(pathname: string, search: AppSearch): EntityPeekState | null {
  const stack = searchList(search.drawer_stack);
  if (stack.length > 0) {
    return { activeId: stack.at(-1)!, source: 'drawer_stack', stack };
  }

  const taskId = optionalString(search.peek_task);
  if (taskId) return { activeId: taskId, source: 'peek_task', stack: [taskId] };

  // Keep supporting existing task-page deep links without interpreting the
  // Activity page's `task` filter as an open entity.
  const legacyTaskId = isTasksPath(pathname) ? optionalString(search.task) : null;
  return legacyTaskId
    ? { activeId: legacyTaskId, source: 'task', stack: [legacyTaskId] }
    : null;
}

export function withEntityPeek(search: AppSearch, id: string): AppSearch {
  return { ...search, drawer_stack: [id], peek_task: undefined };
}

export function pushEntityPeek(pathname: string, search: AppSearch, id: string): AppSearch {
  const current = getEntityPeek(pathname, search);
  if (!current) return withEntityPeek(search, id);
  if (current.activeId === id) return search;

  return {
    ...search,
    drawer_stack: [...current.stack, id],
    peek_task: undefined,
    ...(current.source === 'task' ? { task: undefined } : {}),
  };
}

export function backEntityPeek(pathname: string, search: AppSearch): AppSearch {
  const current = getEntityPeek(pathname, search);
  if (!current) return search;

  if (current.source === 'drawer_stack') {
    return {
      ...search,
      drawer_stack: current.stack.length > 1 ? current.stack.slice(0, -1) : undefined,
    };
  }
  return { ...search, [current.source]: undefined };
}

export function closeEntityPeek(pathname: string, search: AppSearch): AppSearch {
  return {
    ...search,
    drawer_stack: undefined,
    peek_task: undefined,
    ...(isTasksPath(pathname) ? { task: undefined } : {}),
  };
}

export function popEntityPeekTo(search: AppSearch, index: number): AppSearch {
  const stack = searchList(search.drawer_stack);
  if (index < 0 || index >= stack.length - 1) return search;
  return { ...search, drawer_stack: stack.slice(0, index + 1) };
}
