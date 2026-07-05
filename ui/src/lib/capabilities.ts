// Pure capability + nav-gating helpers shared by useMe and AppShell. Kept free
// of React/transport imports so they unit-test in isolation.
import type { Me, MeProject, MemberCapability } from './types';

// Admin (identity 'admin', or the synthetic null we use before /me resolves for
// the bearer flow) can do everything. A member is granted per-project: the
// action must be listed in that project's capabilities.
export function meCan(
  me: Me | null,
  projectId: string | null | undefined,
  action: MemberCapability,
): boolean {
  if (!me || me.identity === 'admin') return true;
  if (!projectId) return false;
  const project = me.projects.find((entry) => entry.projectId === projectId);
  return project ? project.capabilities.includes(action) : false;
}

// The projects a member may see. Admin returns null, meaning "all projects" —
// callers fall back to the full project list they already load.
export function meVisibleProjects(me: Me | null): MeProject[] | null {
  if (!me || me.identity === 'admin') return null;
  return me.projects;
}

// The capability a nav destination requires. Pages absent from this map are
// always visible (e.g. Settings/Status). An artifacts-only member (project.read
// + artifacts.read/comment) therefore sees ONLY Project + Artifacts.
export const NAV_CAPABILITY: Partial<Record<string, MemberCapability>> = {
  project: 'project.read',
  decisions: 'graph.read',
  architecture: 'graph.read',
  glossary: 'graph.read',
  tasks: 'tasks.read',
  artifacts: 'artifacts.read',
  prompts: 'graph.read',
  activity: 'tasks.read',
};

// Pages whose backing routes are admin-only regardless of member capability:
// Activity reads the daemon tx log and Prompts is the authoring studio, neither
// exposed to members. Hide them outright even for a member who holds the read
// capability the page is otherwise keyed on, so members never see nav that 403s.
const MEMBER_HIDDEN_PAGES = new Set(['activity', 'prompts']);

export function navPageVisible(
  me: Me | null,
  projectId: string | null | undefined,
  page: string,
): boolean {
  if (me?.identity === 'member' && MEMBER_HIDDEN_PAGES.has(page)) return false;
  const required = NAV_CAPABILITY[page];
  if (!required) return true;
  return meCan(me, projectId, required);
}
