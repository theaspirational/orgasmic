// @arch arch_MK2Q2.3
import { useCallback } from 'react';
import { useNavigate, useRouterState } from '@tanstack/react-router';

import { openTab } from '@/lib/tabsStore';

const LAST_PROJECT_KEYS = ['orgasmic.lastProject', 'orgasmic.active_project'];

function pathParts(pathname: string): string[] {
  return pathname.split('/').filter(Boolean).map(decodeURIComponent);
}

function projectIdFromPath(pathname: string): string | null {
  const parts = pathParts(pathname);
  return parts[0] === 'projects' ? parts[1] ?? null : null;
}

function readStoredProject(): string | null {
  if (typeof window === 'undefined') return null;
  for (const key of LAST_PROJECT_KEYS) {
    const value = window.localStorage.getItem(key);
    if (value) return value;
  }
  return null;
}

function writeStoredProject(projectId: string) {
  if (typeof window === 'undefined') return;
  for (const key of LAST_PROJECT_KEYS) {
    window.localStorage.setItem(key, projectId);
  }
}

function clearStoredProject() {
  if (typeof window === 'undefined') return;
  for (const key of LAST_PROJECT_KEYS) {
    window.localStorage.removeItem(key);
  }
}

export function useActiveProject() {
  const navigate = useNavigate();
  const pathname = useRouterState({ select: (state) => state.location.pathname });
  const routeProjectId = projectIdFromPath(pathname);
  const activeProjectId = routeProjectId ?? readStoredProject();

  const setActiveProject = useCallback(
    (projectId: string) => {
      writeStoredProject(projectId);
      // Open (or focus) the tab and land on its remembered view.
      const view = openTab(projectId);
      void navigate({
        to: `/projects/$projectId/${view}` as '/projects/$projectId/decisions',
        params: { projectId },
      });
    },
    [navigate],
  );

  const clearActiveProject = useCallback(() => {
    clearStoredProject();
  }, []);

  return { activeProjectId, setActiveProject, clearActiveProject };
}
