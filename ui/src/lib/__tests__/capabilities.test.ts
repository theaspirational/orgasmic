import { describe, expect, it } from 'vitest';

import { meCan, meVisibleProjects, navPageVisible, NAV_CAPABILITY } from '../capabilities';
import type { Me } from '../types';

const admin: Me = { identity: 'admin', name: null, projects: [] };

// The primary member persona: artifacts-only. Can read/comment on artifacts and
// read the project, nothing else.
const artifactsOnly: Me = {
  identity: 'member',
  name: 'Reviewer',
  projects: [
    {
      projectId: 'orgasmic',
      role: 'reviewer',
      capabilities: ['project.read', 'artifacts.read', 'artifacts.comment'],
    },
  ],
};

const watcher: Me = {
  identity: 'member',
  name: 'Watcher',
  projects: [
    {
      projectId: 'orgasmic',
      role: 'observer',
      capabilities: ['project.read', 'sessions.watch'],
    },
  ],
};

describe('meCan', () => {
  it('admin can do everything on any project', () => {
    expect(meCan(admin, 'orgasmic', 'artifacts.generate')).toBe(true);
    expect(meCan(admin, 'anything', 'members.manage')).toBe(true);
    expect(meCan(admin, null, 'graph.read')).toBe(true);
  });

  it('a null snapshot is treated as admin (pre-/me bearer flow)', () => {
    expect(meCan(null, 'orgasmic', 'tasks.read')).toBe(true);
  });

  it('artifacts-only member: true for artifacts, false for graph/tasks/sessions', () => {
    expect(meCan(artifactsOnly, 'orgasmic', 'artifacts.read')).toBe(true);
    expect(meCan(artifactsOnly, 'orgasmic', 'artifacts.comment')).toBe(true);
    expect(meCan(artifactsOnly, 'orgasmic', 'project.read')).toBe(true);
    expect(meCan(artifactsOnly, 'orgasmic', 'artifacts.generate')).toBe(false);
    expect(meCan(artifactsOnly, 'orgasmic', 'graph.read')).toBe(false);
    expect(meCan(artifactsOnly, 'orgasmic', 'tasks.read')).toBe(false);
    expect(meCan(artifactsOnly, 'orgasmic', 'sessions.watch')).toBe(false);
    expect(meCan(artifactsOnly, 'orgasmic', 'sessions.interact')).toBe(false);
  });

  it('member has no capability on a project they were not granted', () => {
    expect(meCan(artifactsOnly, 'other-project', 'artifacts.read')).toBe(false);
  });

  it('member with a null projectId is denied', () => {
    expect(meCan(artifactsOnly, null, 'artifacts.read')).toBe(false);
  });
});

describe('meVisibleProjects', () => {
  it('admin returns null (meaning all projects)', () => {
    expect(meVisibleProjects(admin)).toBeNull();
    expect(meVisibleProjects(null)).toBeNull();
  });

  it('member returns only their granted projects', () => {
    const visible = meVisibleProjects(artifactsOnly);
    expect(visible?.map((p) => p.projectId)).toEqual(['orgasmic']);
  });
});

describe('navPageVisible (AppShell nav gating)', () => {
  // The eight nav destinations AppShell filters (PRIMARY + MORE).
  const navPages = ['project', 'decisions', 'architecture', 'tasks', 'glossary', 'artifacts', 'prompts', 'activity'];

  function visibleFor(me: Me) {
    return navPages.filter((page) => navPageVisible(me, 'orgasmic', page));
  }

  it('admin sees every nav destination', () => {
    expect(visibleFor(admin)).toEqual(navPages);
  });

  it('artifacts-only member sees ONLY Project + Artifacts', () => {
    expect(visibleFor(artifactsOnly).sort()).toEqual(['artifacts', 'project']);
  });

  it('a watch-only member sees Project (graph/tasks/artifacts hidden)', () => {
    expect(visibleFor(watcher)).toEqual(['project']);
  });

  it('every nav page maps to a required capability', () => {
    for (const page of navPages) {
      expect(NAV_CAPABILITY[page]).toBeDefined();
    }
  });
});
