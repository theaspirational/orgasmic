// @arch arch_MK2Q2.1
import { getQueryParam } from '../lib/routing';
import type { ViewName } from '../lib/types';

export function initialView(): ViewName {
  const raw = getQueryParam('view');
  if (
    raw === 'board' ||
    raw === 'decisions' ||
    raw === 'architecture' ||
    raw === 'glossary' ||
    raw === 'activity' ||
    raw === 'project' ||
    raw === 'tasks' ||
    raw === 'task' ||
    raw === 'runs' ||
    raw === 'prompts' ||
    raw === 'manager' ||
    raw === 'org' ||
    raw === 'status' ||
    raw === 'settings'
  ) {
    return raw;
  }
  return 'board';
}

export function initialTasksLayout(): 'list' | 'kanban' {
  const raw = getQueryParam('layout');
  return raw === 'kanban' ? 'kanban' : 'list';
}

export function initialProject(): string | null {
  return getQueryParam('project');
}

export function initialTask(): string | null {
  return getQueryParam('task');
}

export const NAV: { id: ViewName; label: string }[] = [
  { id: 'board', label: 'Board' },
  { id: 'decisions', label: 'Decisions' },
  { id: 'architecture', label: 'Architecture' },
  { id: 'glossary', label: 'Glossary' },
  { id: 'activity', label: 'Activity' },
  { id: 'project', label: 'Project' },
  { id: 'tasks', label: 'Tasks' },
  { id: 'task', label: 'Task' },
  { id: 'runs', label: 'Runs' },
  { id: 'prompts', label: 'Prompts' },
  { id: 'manager', label: 'Manager' },
  { id: 'org', label: 'Org' },
  { id: 'status', label: 'Status' },
  { id: 'settings', label: 'Settings' },
];
