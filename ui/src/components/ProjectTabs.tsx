import { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate } from '@tanstack/react-router';
import { Plus, X } from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuTrigger,
} from '@/components/ui/context-menu';
import { Input } from '@/components/ui/input';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import { Skeleton } from '@/components/ui/skeleton';
import { useActiveProject } from '@/hooks/useActiveProject';
import { useProjectTabs } from '@/hooks/useProjectTabs';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchProjects } from '@/lib/api';
import { projectChipStyle, projectInitial } from '@/lib/projectColor';
import { DEFAULT_TAB_VIEW, type ProjectTab, type TabView } from '@/lib/tabsStore';
import type { ProjectIndex } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import { ProjectAddDialog } from './ProjectAddDialog';

const VIEW_LABELS: Record<TabView, string> = {
  decisions: 'Decisions',
  architecture: 'Architecture',
  tasks: 'Tasks',
  glossary: 'Glossary',
  project: 'Project',
  runs: 'Runs',
  prompts: 'Prompts',
  org: 'Org',
  activity: 'Activity',
  status: 'Status',
  settings: 'Settings',
};

type NavTarget = {
  to: '/projects/$projectId/decisions';
  params: { projectId: string };
};

// Every project view is a real route that takes `projectId`; cast the dynamic
// view onto one route literal so TanStack's types are satisfied (the runtime
// `to` string is what actually drives navigation).
function navTarget(projectId: string, view: TabView): NavTarget {
  return { to: `/projects/$projectId/${view}` as NavTarget['to'], params: { projectId } };
}

function focusSibling(el: HTMLElement, dir: 1 | -1) {
  const strip = el.closest('[data-tabstrip]');
  if (!strip) return;
  const items = Array.from(strip.querySelectorAll<HTMLElement>('[data-project-tab]'));
  const index = items.indexOf(el);
  items[index + dir]?.focus();
}

export function ProjectTabs() {
  const navigate = useNavigate();
  const refresh = useRefreshToken();
  const { activeProjectId } = useActiveProject();
  const { tabs, openTab, closeTab, closeOthers, closeToRight, reorderTabs, pruneTabs } =
    useProjectTabs();
  const { data: projects } = useResource(`projects:${refresh}:tabs`, fetchProjects);

  const projectMeta = useMemo(() => {
    const map = new Map<string, ProjectIndex>();
    for (const project of projects ?? []) map.set(project.project_id, project);
    return map;
  }, [projects]);

  // Reconcile the persisted tab list against the live project list.
  useEffect(() => {
    if (projects) pruneTabs(projects.map((project) => project.project_id));
  }, [projects, pruneTabs]);

  const [dragIndex, setDragIndex] = useState<number | null>(null);
  const [overIndex, setOverIndex] = useState<number | null>(null);

  const goTo = useCallback(
    (projectId: string, view: TabView) => void navigate(navTarget(projectId, view)),
    [navigate],
  );

  const handleSelect = useCallback(
    (tab: ProjectTab) => {
      if (tab.projectId !== activeProjectId) goTo(tab.projectId, tab.view);
    },
    [activeProjectId, goTo],
  );

  const handleClose = useCallback(
    (projectId: string) => {
      const neighborId = closeTab(projectId);
      if (projectId !== activeProjectId) return;
      if (!neighborId) {
        void navigate({ to: '/board' });
        return;
      }
      const view = tabs.find((tab) => tab.projectId === neighborId)?.view ?? DEFAULT_TAB_VIEW;
      goTo(neighborId, view);
    },
    [activeProjectId, closeTab, goTo, navigate, tabs],
  );

  const handleCloseOthers = useCallback(
    (projectId: string) => {
      closeOthers(projectId);
      if (activeProjectId !== projectId) {
        const view = tabs.find((tab) => tab.projectId === projectId)?.view ?? DEFAULT_TAB_VIEW;
        goTo(projectId, view);
      }
    },
    [activeProjectId, closeOthers, goTo, tabs],
  );

  const handleCloseToRight = useCallback(
    (projectId: string) => {
      const index = tabs.findIndex((tab) => tab.projectId === projectId);
      const activeIndex = tabs.findIndex((tab) => tab.projectId === activeProjectId);
      closeToRight(projectId);
      if (activeIndex > index && index >= 0) {
        goTo(projectId, tabs[index]?.view ?? DEFAULT_TAB_VIEW);
      }
    },
    [activeProjectId, closeToRight, goTo, tabs],
  );

  const handleDrop = useCallback(
    (index: number) => {
      if (dragIndex !== null) reorderTabs(dragIndex, index);
      setDragIndex(null);
      setOverIndex(null);
    },
    [dragIndex, reorderTabs],
  );

  const resetDrag = useCallback(() => {
    setDragIndex(null);
    setOverIndex(null);
  }, []);

  return (
    <nav
      aria-label="Open projects"
      data-tabstrip
      className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto px-1 py-1 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
    >
      {tabs.map((tab, index) => (
        <ProjectTabItem
          key={tab.projectId}
          tab={tab}
          meta={projectMeta.get(tab.projectId) ?? null}
          active={tab.projectId === activeProjectId}
          canCloseOthers={tabs.length > 1}
          canCloseToRight={index < tabs.length - 1}
          dragging={dragIndex === index}
          dropTarget={dragIndex !== null && overIndex === index && dragIndex !== index}
          onSelect={() => handleSelect(tab)}
          onClose={() => handleClose(tab.projectId)}
          onCloseOthers={() => handleCloseOthers(tab.projectId)}
          onCloseToRight={() => handleCloseToRight(tab.projectId)}
          onDragStart={() => setDragIndex(index)}
          onDragOver={() => setOverIndex(index)}
          onDrop={() => handleDrop(index)}
          onDragEnd={resetDrag}
        />
      ))}
      <NewTabMenu
        projects={projects}
        openProjectIds={tabs.map((tab) => tab.projectId)}
        onOpenProject={(projectId) => goTo(projectId, openTab(projectId))}
      />
    </nav>
  );
}

function ProjectTabItem({
  tab,
  meta,
  active,
  canCloseOthers,
  canCloseToRight,
  dragging,
  dropTarget,
  onSelect,
  onClose,
  onCloseOthers,
  onCloseToRight,
  onDragStart,
  onDragOver,
  onDrop,
  onDragEnd,
}: {
  tab: ProjectTab;
  meta: ProjectIndex | null;
  active: boolean;
  canCloseOthers: boolean;
  canCloseToRight: boolean;
  dragging: boolean;
  dropTarget: boolean;
  onSelect: () => void;
  onClose: () => void;
  onCloseOthers: () => void;
  onCloseToRight: () => void;
  onDragStart: () => void;
  onDragOver: () => void;
  onDrop: () => void;
  onDragEnd: () => void;
}) {
  const viewLabel = VIEW_LABELS[tab.view];
  const taskCount = meta?.tasks.length;
  const title = meta
    ? `${tab.projectId} · ${meta.branch}${
        taskCount != null ? ` · ${taskCount} task${taskCount === 1 ? '' : 's'}` : ''
      }`
    : tab.projectId;

  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <div
          data-project-tab
          role="button"
          tabIndex={0}
          aria-current={active ? 'page' : undefined}
          aria-label={`${tab.projectId}, ${viewLabel}`}
          title={title}
          draggable
          onClick={onSelect}
          onAuxClick={(event) => {
            if (event.button === 1) {
              event.preventDefault();
              onClose();
            }
          }}
          onKeyDown={(event) => {
            if (event.key === 'Enter' || event.key === ' ') {
              event.preventDefault();
              onSelect();
            } else if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') {
              event.preventDefault();
              focusSibling(event.currentTarget, event.key === 'ArrowRight' ? 1 : -1);
            } else if (event.key === 'Delete' || event.key === 'Backspace') {
              event.preventDefault();
              onClose();
            }
          }}
          onDragStart={(event) => {
            event.dataTransfer.effectAllowed = 'move';
            event.dataTransfer.setData('text/plain', tab.projectId);
            onDragStart();
          }}
          onDragOver={(event) => {
            event.preventDefault();
            event.dataTransfer.dropEffect = 'move';
            onDragOver();
          }}
          onDrop={(event) => {
            event.preventDefault();
            onDrop();
          }}
          onDragEnd={onDragEnd}
          className={cn(
            'group/tab relative flex h-11 min-w-[8.5rem] max-w-[15rem] shrink-0 cursor-pointer select-none items-center gap-2 rounded-lg px-2.5 outline-none transition-[background-color,box-shadow,color,opacity] duration-150 ease-out focus-visible:ring-2 focus-visible:ring-ring motion-reduce:transition-none',
            active
              ? 'bg-card text-foreground shadow-sm ring-1 ring-primary/30'
              : 'text-muted-foreground hover:bg-muted hover:text-foreground',
            dragging && 'opacity-50',
            dropTarget && 'bg-muted',
          )}
        >
          <span
            className="flex size-5 shrink-0 items-center justify-center rounded-[5px] text-[10px] font-semibold leading-none"
            style={projectChipStyle(tab.projectId)}
            aria-hidden="true"
          >
            {projectInitial(tab.projectId)}
          </span>
          <span className="flex min-w-0 flex-col">
            <span className={cn('truncate font-mono text-[13px] leading-tight', active && 'font-medium')}>
              {tab.projectId}
            </span>
            <span className="truncate text-[11px] leading-tight text-muted-foreground">{viewLabel}</span>
          </span>
          <button
            type="button"
            draggable={false}
            tabIndex={-1}
            aria-label={`Close ${tab.projectId}`}
            onClick={(event) => {
              event.stopPropagation();
              onClose();
            }}
            onPointerDown={(event) => event.stopPropagation()}
            className={cn(
              'ml-auto flex size-5 shrink-0 items-center justify-center rounded-md text-muted-foreground opacity-0 transition-opacity hover:bg-foreground/10 hover:text-foreground group-hover/tab:opacity-100 group-focus-within/tab:opacity-100 motion-reduce:transition-none',
              active && 'opacity-70',
            )}
          >
            <X className="size-3.5" />
          </button>
        </div>
      </ContextMenuTrigger>
      <ContextMenuContent>
        <ContextMenuItem onSelect={onClose}>Close tab</ContextMenuItem>
        <ContextMenuItem onSelect={onCloseOthers} disabled={!canCloseOthers}>
          Close others
        </ContextMenuItem>
        <ContextMenuItem onSelect={onCloseToRight} disabled={!canCloseToRight}>
          Close to the right
        </ContextMenuItem>
      </ContextMenuContent>
    </ContextMenu>
  );
}

function NewTabMenu({
  projects,
  openProjectIds,
  onOpenProject,
}: {
  projects: ProjectIndex[] | null;
  openProjectIds: string[];
  onOpenProject: (projectId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [addOpen, setAddOpen] = useState(false);

  const openSet = useMemo(() => new Set(openProjectIds), [openProjectIds]);
  const available = useMemo(
    () => (projects ?? []).filter((project) => !openSet.has(project.project_id)),
    [projects, openSet],
  );
  const normalized = query.trim().toLowerCase();
  const visible = useMemo(
    () =>
      normalized
        ? available.filter((project) =>
            `${project.project_id} ${project.repo_url}`.toLowerCase().includes(normalized),
          )
        : available,
    [available, normalized],
  );
  const showFilter = available.length > 8;

  const emptyMessage =
    available.length === 0
      ? projects && projects.length === 0
        ? 'No projects yet.'
        : 'All projects are open.'
      : 'No matching projects.';

  return (
    <>
      <Popover
        open={open}
        onOpenChange={(next) => {
          setOpen(next);
          if (!next) setQuery('');
        }}
      >
        <PopoverTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            className="size-9 shrink-0 text-muted-foreground"
            aria-label="Open a project in a new tab"
          >
            <Plus />
          </Button>
        </PopoverTrigger>
        <PopoverContent align="start" sideOffset={6} className="w-72 p-2">
          {showFilter ? (
            <Input
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="Filter projects"
              className="mb-2 h-8"
              autoFocus
            />
          ) : null}
          {projects === null ? (
            <div className="flex flex-col gap-1">
              {[0, 1, 2].map((item) => (
                <Skeleton key={item} className="h-9" />
              ))}
            </div>
          ) : visible.length === 0 ? (
            <div className="px-2 py-6 text-center text-sm text-muted-foreground">{emptyMessage}</div>
          ) : (
            <div className="max-h-72 overflow-y-auto">
              {visible.map((project) => (
                <button
                  key={project.project_id}
                  type="button"
                  className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm outline-none hover:bg-accent focus-visible:bg-accent"
                  onClick={() => {
                    onOpenProject(project.project_id);
                    setOpen(false);
                    setQuery('');
                  }}
                >
                  <span
                    className="flex size-5 shrink-0 items-center justify-center rounded-[5px] text-[10px] font-semibold leading-none"
                    style={projectChipStyle(project.project_id)}
                    aria-hidden="true"
                  >
                    {projectInitial(project.project_id)}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate font-mono">{project.project_id}</span>
                    <span className="block truncate font-mono text-xs text-muted-foreground">
                      {project.branch}
                    </span>
                  </span>
                </button>
              ))}
            </div>
          )}
          <div className="mt-1 border-t pt-1">
            <button
              type="button"
              className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm outline-none hover:bg-accent focus-visible:bg-accent"
              onClick={() => {
                setOpen(false);
                setAddOpen(true);
              }}
            >
              <Plus className="size-4" />
              <span>Add new project</span>
            </button>
          </div>
        </PopoverContent>
      </Popover>
      <ProjectAddDialog open={addOpen} onOpenChange={setAddOpen} />
    </>
  );
}
