// @vitest-environment jsdom
import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ProjectCatalogEntry } from '@/lib/types';

const mocks = vi.hoisted(() => ({
  fetchProjects: vi.fn(),
  navigate: vi.fn(),
  onSelectProject: vi.fn(),
  openTab: vi.fn(() => 'tasks'),
  closeTab: vi.fn(),
  closeOthers: vi.fn(),
  closeToRight: vi.fn(),
  reorderTabs: vi.fn(),
  pruneTabs: vi.fn(),
}));

vi.mock('@/lib/api', async (importOriginal) => ({
  ...(await importOriginal<typeof import('@/lib/api')>()),
  fetchProjects: mocks.fetchProjects,
}));

vi.mock('@tanstack/react-router', () => ({
  useNavigate: () => mocks.navigate,
}));

vi.mock('@/hooks/useActiveProject', () => ({
  useActiveProject: () => ({ activeProjectId: 'proj-loading' }),
}));

vi.mock('@/hooks/useProjectTabs', () => ({
  useProjectTabs: () => ({
    tabs: [{ projectId: 'proj-loading', view: 'tasks' }],
    openTab: mocks.openTab,
    closeTab: mocks.closeTab,
    closeOthers: mocks.closeOthers,
    closeToRight: mocks.closeToRight,
    reorderTabs: mocks.reorderTabs,
    pruneTabs: mocks.pruneTabs,
  }),
}));

vi.mock('@/hooks/useMe', () => ({
  useMe: () => ({ isMember: false }),
}));

vi.mock('../ProjectAddDialog', () => ({
  ProjectAddDialog: () => null,
}));

vi.mock('../ProjectsManageDialog', () => ({
  ProjectsManageDialog: () => null,
}));

import { BoardView } from '../BoardView';
import { ProjectTabs } from '../ProjectTabs';

function project(
  projectId: string,
  state: ProjectCatalogEntry['load']['state'],
  overrides: Partial<ProjectCatalogEntry> = {},
): ProjectCatalogEntry {
  return {
    project_id: projectId,
    root: `/work/${projectId}`,
    repo_url: '',
    branch: 'main',
    status: 'active',
    load: { state, generation: 0 },
    task_stats: null,
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
});

afterEach(cleanup);

describe('catalog-driven project discovery', () => {
  it('keeps unloaded and failed projects navigable without invented task counts', async () => {
    mocks.fetchProjects.mockResolvedValue([
      project('proj-unloaded', 'unloaded'),
      project('proj-failed', 'failed', {
        load: { state: 'failed', generation: 0, error: 'permission denied' },
      }),
    ]);

    render(<BoardView onSelectProject={mocks.onSelectProject} />);

    expect(await screen.findByText('proj-unloaded')).toBeInTheDocument();
    expect(screen.getByText('proj-failed')).toBeInTheDocument();
    expect(screen.getAllByText('Tasks not loaded')).toHaveLength(2);
    expect(screen.getByText('permission denied')).toBeInTheDocument();

    fireEvent.click(screen.getByText('proj-unloaded'));
    expect(mocks.onSelectProject).toHaveBeenCalledWith('proj-unloaded');
  });

  it('renders a loading project tab from catalog metadata', async () => {
    mocks.fetchProjects.mockResolvedValue([project('proj-loading', 'loading')]);

    render(<ProjectTabs />);

    const tab = await screen.findByRole('button', { name: 'proj-loading, Tasks' });
    expect(tab).toHaveAttribute('title', 'proj-loading · main · loading');
  });

  it('retains blocked-task pressure on ready project cards', async () => {
    mocks.fetchProjects.mockResolvedValue([
      project('proj-ready', 'ready', {
        task_stats: { total: 7, active: 4, blocked: 2, done: 3 },
      }),
    ]);

    render(<BoardView onSelectProject={mocks.onSelectProject} />);

    expect(await screen.findByText('2 blocked')).toBeInTheDocument();
    expect(screen.getByText('4 active')).toBeInTheDocument();
    expect(screen.getByText('3 done')).toBeInTheDocument();
  });

  it('uses destructive diagnostics only for genuine project load failures', async () => {
    mocks.fetchProjects.mockResolvedValue([
      project('proj-delayed', 'delayed', {
        load: { state: 'delayed', generation: 0, error: 'queued behind slow scans' },
      }),
      project('proj-ready', 'ready', {
        load: { state: 'ready', generation: 1, error: 'last refresh was delayed' },
      }),
      project('proj-failed', 'failed', {
        load: { state: 'failed', generation: 0, error: 'permission denied' },
      }),
    ]);

    render(<BoardView onSelectProject={mocks.onSelectProject} />);

    expect(await screen.findByText('queued behind slow scans')).toHaveClass('text-muted-foreground');
    expect(screen.getByText('last refresh was delayed')).toHaveClass('text-muted-foreground');
    expect(screen.getByText('permission denied')).toHaveClass('text-destructive');
  });
});
