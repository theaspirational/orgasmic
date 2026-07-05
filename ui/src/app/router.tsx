// @arch arch_MK2Q2.1
import { lazy, Suspense, useEffect } from 'react';
import {
  createRootRoute,
  createRoute,
  createRouter,
  defaultParseSearch,
  defaultStringifySearch,
} from '@tanstack/react-router';

import { AppShell } from '@/components/AppShell';
import { BoardRouteView } from '@/components/BoardRouteView';
import { ErrorPanel, Loading } from '@/components/Primitives';
import { ProjectView } from '@/components/ProjectView';
import { RunsView } from '@/components/RunsView';
import { SettingsView } from '@/components/SettingsView';
import { StatusView } from '@/components/StatusView';
import { TasksPage } from '@/components/TasksPage';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Skeleton } from '@/components/ui/skeleton';
import { fetchProjects } from '@/lib/api';
import { routeSearch } from '@/lib/searchState';
import { DEFAULT_TAB_VIEW, getSnapshot as getTabsSnapshot } from '@/lib/tabsStore';
import type { Me, ProjectIndex } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { useMe } from '@/hooks/useMe';
import { navPageVisible } from '@/lib/capabilities';

const ActivityView = lazy(() =>
  import('@/components/ActivityView').then((module) => ({ default: module.ActivityView })),
);
const ArchitectureView = lazy(() =>
  import('@/components/ArchitectureView').then((module) => ({ default: module.ArchitectureView })),
);
const ArtifactsView = lazy(() =>
  import('@/components/ArtifactsView').then((module) => ({ default: module.ArtifactsView })),
);
const ArtifactView = lazy(() =>
  import('@/components/ArtifactView').then((module) => ({ default: module.ArtifactView })),
);
const DecisionsView = lazy(() =>
  import('@/components/DecisionsView').then((module) => ({ default: module.DecisionsView })),
);
const GlossaryView = lazy(() =>
  import('@/components/GlossaryView').then((module) => ({ default: module.GlossaryView })),
);
const OrgView = lazy(() =>
  import('@/components/OrgView').then((module) => ({ default: module.OrgView })),
);
const PromptStudioView = lazy(() =>
  import('@/components/PromptStudioView').then((module) => ({ default: module.PromptStudioView })),
);
const TaskDialog = lazy(() =>
  import('@/components/TaskDialog').then((module) => ({ default: module.TaskDialog })),
);

const LAST_PROJECT_KEYS = ['orgasmic.lastProject', 'orgasmic.active_project'];
const DEFAULT_PROJECT_ID = 'orgasmic';

function readLastProject(): string | null {
  if (typeof window === 'undefined') return null;
  for (const key of LAST_PROJECT_KEYS) {
    const value = window.localStorage.getItem(key);
    if (value) return value;
  }
  return null;
}

function rememberProject(projectId: string) {
  if (typeof window === 'undefined') return;
  window.localStorage.setItem('orgasmic.lastProject', projectId);
  window.localStorage.setItem('orgasmic.active_project', projectId);
}

function chooseInitialProject(projects: ProjectIndex[]): string | null {
  const remembered = readLastProject();
  if (remembered && projects.some((project) => project.project_id === remembered)) {
    return remembered;
  }
  if (projects.some((project) => project.project_id === DEFAULT_PROJECT_ID)) {
    return DEFAULT_PROJECT_ID;
  }
  return projects[0]?.project_id ?? null;
}

// The first project view a member may land on, in priority order. A member is
// routed here from `/` instead of the admin Decisions default, so an
// artifacts-only member (no graph.read) lands on Artifacts rather than a view
// their capabilities would 403.
const MEMBER_LANDING_VIEWS = ['decisions', 'tasks', 'artifacts', 'project'] as const;

function memberLandingView(me: Me | null, projectId: string): string {
  for (const view of MEMBER_LANDING_VIEWS) {
    if (navPageVisible(me, projectId, view)) return view;
  }
  return 'project';
}

function fallback(label: string) {
  return <Loading label={label} />;
}

type SearchRecord = Record<string, unknown>;
type ManagerSearchSize = 'peek' | 'workbench' | 'focus';
type ActivityRange = 'today' | '7d' | '30d' | 'custom' | 'all';
type DrawerSearch = { drawer_stack?: string[] };
type RootSearch = { manager?: ManagerSearchSize };
type DecisionsSearch = DrawerSearch & { tag?: string[]; q?: string };
type ArchitectureSearch = DrawerSearch & { q?: string };
type GlossarySearch = DrawerSearch & { q?: string };
type TasksLayoutSearch = 'list' | 'kanban';
type TasksSearch = { task?: string; layout?: TasksLayoutSearch };
type ActivitySearch = {
  types?: string[];
  actors?: string[];
  range?: ActivityRange;
  from?: string;
  to?: string;
  task?: string;
};

const LIST_SEARCH_KEYS = new Set(['drawer_stack', 'tag', 'types', 'actors']);

function readString(value: unknown): string {
  if (value === null || value === undefined || Array.isArray(value) || typeof value === 'object') return '';
  return String(value);
}

function readOptionalString(value: unknown): string | undefined {
  const text = readString(value).trim();
  return text || undefined;
}

function readStringList(value: unknown): string[] {
  if (Array.isArray(value)) {
    return value.map((item) => String(item).trim()).filter(Boolean);
  }
  if (typeof value === 'string') {
    return value.split(',').map((part) => part.trim()).filter(Boolean);
  }
  return [];
}

function readManager(value: unknown): ManagerSearchSize | undefined {
  return value === 'peek' || value === 'workbench' || value === 'focus' ? value : undefined;
}

function readActivityRange(value: unknown): ActivityRange {
  return value === 'today' || value === '7d' || value === '30d' || value === 'custom' || value === 'all'
    ? value
    : '30d';
}

function readTasksLayout(value: unknown): TasksLayoutSearch | undefined {
  return value === 'list' || value === 'kanban' ? value : undefined;
}

function drawerSearch(raw: SearchRecord): DrawerSearch {
  const drawerStack = readStringList(raw.drawer_stack);
  return drawerStack.length > 0 ? { drawer_stack: drawerStack } : {};
}

function decisionsSearch(raw: SearchRecord): DecisionsSearch {
  const tag = readStringList(raw.tag);
  const q = readString(raw.q);
  return {
    ...drawerSearch(raw),
    ...(tag.length > 0 ? { tag } : {}),
    ...(q ? { q } : {}),
  };
}

function architectureSearch(raw: SearchRecord): ArchitectureSearch {
  const q = readString(raw.q);
  return {
    ...drawerSearch(raw),
    ...(q ? { q } : {}),
  };
}

function glossarySearch(raw: SearchRecord): GlossarySearch {
  const q = readString(raw.q);
  return {
    ...drawerSearch(raw),
    ...(q ? { q } : {}),
  };
}

type ArtifactViewSearch = { version?: number };

function artifactViewSearch(raw: SearchRecord): ArtifactViewSearch {
  const version = Number(raw.version);
  return Number.isFinite(version) && version > 0 ? { version } : {};
}

function tasksSearch(raw: SearchRecord): TasksSearch {
  const task = readOptionalString(raw.task);
  const layout = readTasksLayout(raw.layout);
  return {
    ...(task ? { task } : {}),
    ...(layout ? { layout } : {}),
  };
}

function TaskDialogChunkFallback({
  taskId,
  onClose,
}: {
  taskId: string;
  onClose: () => void;
}) {
  return (
    <Dialog open onOpenChange={(next) => !next && onClose()}>
      <DialogContent
        showCloseButton
        className="grid h-[min(90vh,46rem)] w-[min(96vw,80rem)] max-w-none grid-rows-[auto_1fr] gap-0 overflow-hidden p-0 sm:max-w-none"
      >
        <DialogHeader className="border-b px-5 py-4 pr-12">
          <DialogDescription className="font-mono text-xs">{taskId}</DialogDescription>
          <DialogTitle className="text-base font-semibold leading-snug sm:text-lg">
            Loading task
          </DialogTitle>
        </DialogHeader>
        <div className="grid min-h-0 grid-cols-1 md:grid-cols-[16rem_minmax(0,1fr)_18rem]">
          <div className="hidden border-r bg-muted/20 p-3 md:block">
            <Skeleton className="h-8" />
          </div>
          <div className="flex flex-col gap-4 px-5 py-5">
            <Skeleton className="h-4 w-40" />
            <Skeleton className="h-24" />
          </div>
          <div className="hidden border-l bg-muted/20 p-3 lg:block">
            <Skeleton className="h-20" />
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function activitySearch(raw: SearchRecord): ActivitySearch {
  const types = readStringList(raw.types);
  const actors = readStringList(raw.actors);
  const range = readActivityRange(raw.range);
  const from = readOptionalString(raw.from);
  const to = readOptionalString(raw.to);
  const task = readOptionalString(raw.task);
  return {
    ...(types.length > 0 ? { types } : {}),
    ...(actors.length > 0 ? { actors } : {}),
    ...(range !== '30d' ? { range } : {}),
    ...(from ? { from } : {}),
    ...(to ? { to } : {}),
    ...(task ? { task } : {}),
  };
}

function parseAppSearch(search: string): SearchRecord {
  return defaultParseSearch(search) as SearchRecord;
}

function stringifyAppSearch(search: SearchRecord): string {
  const next: SearchRecord = { ...search };
  for (const key of Object.keys(next)) {
    const value = next[key];
    if (LIST_SEARCH_KEYS.has(key) && Array.isArray(value)) {
      next[key] = value.length > 0 ? value.join(',') : undefined;
      continue;
    }
    if (value === null || value === undefined || value === '' || value === false) {
      next[key] = undefined;
    }
  }
  return defaultStringifySearch(next);
}

const rootRoute = createRootRoute({
  validateSearch: (raw: SearchRecord): RootSearch => {
    const manager = readManager(raw.manager);
    return manager ? { manager } : {};
  },
  component: AppShell,
});

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: '/',
  component: function IndexRoute() {
    const navigate = indexRoute.useNavigate();
    const { isMember, me, visibleProjects } = useMe();
    // A member authenticates by cookie and has no access to the admin `/projects`
    // list — skip that fetch and route from their `/me`-granted projects instead.
    const projects = useResource('projects:index', fetchProjects, { enabled: !isMember });

    useEffect(() => {
      if (isMember) {
        // Wait for /me to resolve before routing — a null snapshot means "not
        // loaded yet", not "no projects", and navigating away from `/` now would
        // unmount this route before the retry could land the member correctly.
        if (!me) return;
        // Land on the first project the member can see, on the first view their
        // capabilities allow (an artifacts-only member lands on Artifacts, never
        // the Decisions default they cannot read).
        const projectId = visibleProjects?.[0]?.projectId ?? null;
        if (!projectId) {
          void navigate({ to: '/board', replace: true });
          return;
        }
        rememberProject(projectId);
        const view = memberLandingView(me, projectId);
        void navigate({
          to: `/projects/$projectId/${view}` as '/projects/$projectId/tasks',
          params: { projectId },
          replace: true,
        });
        return;
      }
      if (!projects.data) return;
      const projectId = chooseInitialProject(projects.data);
      if (!projectId) {
        void navigate({ to: '/board', replace: true });
        return;
      }
      rememberProject(projectId);
      // Restore the project's last view (session restore) instead of always tasks.
      const view = getTabsSnapshot().lastView[projectId] ?? DEFAULT_TAB_VIEW;
      void navigate({
        to: `/projects/$projectId/${view}` as '/projects/$projectId/tasks',
        params: { projectId },
        replace: true,
      });
    }, [navigate, isMember, me, visibleProjects, projects.data]);

    if (!isMember && projects.error) return <ErrorPanel error={projects.error} />;
    return fallback('Loading projects...');
  },
});

const boardRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: 'board',
  component: function BoardRoute() {
    return <BoardRouteView />;
  },
});

const projectRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: 'projects/$projectId',
});

const projectIndexRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: '/',
  validateSearch: decisionsSearch,
  component: function ProjectIndexRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading decisions...')}>
        <DecisionsView projectId={projectId} />
      </Suspense>
    );
  },
});

const projectOverviewRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'project',
  component: function ProjectOverviewRoute() {
    const { projectId } = projectRoute.useParams();
    const navigate = projectOverviewRoute.useNavigate();
    rememberProject(projectId);
    return (
      <ProjectView
        projectId={projectId}
        onSelectTask={(taskId) => {
          void navigate({
            to: '/projects/$projectId/tasks',
            params: { projectId },
            search: routeSearch((prev) => ({
              ...prev,
              task: taskId,
            })),
          });
        }}
      />
    );
  },
});

const decisionsRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'decisions',
  validateSearch: decisionsSearch,
  component: function DecisionsRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading decisions...')}>
        <DecisionsView projectId={projectId} />
      </Suspense>
    );
  },
});

const architectureRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'architecture',
  validateSearch: architectureSearch,
  component: function ArchitectureRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading architecture...')}>
        <ArchitectureView projectId={projectId} />
      </Suspense>
    );
  },
});

const glossaryRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'glossary',
  validateSearch: glossarySearch,
  component: function GlossaryRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading glossary...')}>
        <GlossaryView projectId={projectId} />
      </Suspense>
    );
  },
});

const artifactsRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'artifacts',
  component: function ArtifactsRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading artifacts...')}>
        <ArtifactsView projectId={projectId} />
      </Suspense>
    );
  },
});

const artifactViewRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'artifacts/$artifactId',
  validateSearch: artifactViewSearch,
  component: function ArtifactViewRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading artifact...')}>
        <ArtifactView projectId={projectId} />
      </Suspense>
    );
  },
});

const tasksRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'tasks',
  validateSearch: tasksSearch,
  component: function TasksRoute() {
    const { projectId } = projectRoute.useParams();
    const { task } = tasksRoute.useSearch();
    const navigate = tasksRoute.useNavigate();
    rememberProject(projectId);
    return (
      <>
        <TasksPage
          projectId={projectId}
          onSelectTask={(nextTaskId) => {
            void navigate({
              to: '/projects/$projectId/tasks',
              params: { projectId },
              search: routeSearch((prev) => ({
                ...prev,
                task: nextTaskId,
              })),
            });
          }}
        />
        {task ? (
          <Suspense
            fallback={
              <TaskDialogChunkFallback
                taskId={task}
                onClose={() => {
                  void navigate({
                    to: '/projects/$projectId/tasks',
                    params: { projectId },
                    search: routeSearch((prev) => ({
                      ...prev,
                      task: undefined,
                    })),
                  });
                }}
              />
            }
          >
            <TaskDialog
              projectId={projectId}
              taskId={task}
              onClose={() => {
                void navigate({
                  to: '/projects/$projectId/tasks',
                  params: { projectId },
                  search: routeSearch((prev) => ({
                    ...prev,
                    task: undefined,
                  })),
                });
              }}
              onSelectTask={(nextTaskId) => {
                void navigate({
                  to: '/projects/$projectId/tasks',
                  params: { projectId },
                  search: routeSearch((prev) => ({
                    ...prev,
                    task: nextTaskId,
                  })),
                });
              }}
            />
          </Suspense>
        ) : null}
      </>
    );
  },
});

const activityRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'activity',
  validateSearch: activitySearch,
  component: function ActivityRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading activity...')}>
        <ActivityView projectId={projectId} />
      </Suspense>
    );
  },
});

const runsRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'runs',
  component: function RunsRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return <RunsView projectId={projectId} />;
  },
});

const promptsRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'prompts',
  component: function PromptsRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading prompts...')}>
        <PromptStudioView projectId={projectId} />
      </Suspense>
    );
  },
});

const orgRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'org',
  component: function OrgRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return (
      <Suspense fallback={fallback('Loading Org editor...')}>
        <OrgView projectId={projectId} />
      </Suspense>
    );
  },
});

const settingsRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'settings',
  component: function SettingsRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return <SettingsView projectId={projectId} />;
  },
});

const statusRoute = createRoute({
  getParentRoute: () => projectRoute,
  path: 'status',
  component: function StatusRoute() {
    const { projectId } = projectRoute.useParams();
    rememberProject(projectId);
    return <StatusView />;
  },
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  boardRoute,
  projectRoute.addChildren([
    projectIndexRoute,
    projectOverviewRoute,
    decisionsRoute,
    architectureRoute,
    glossaryRoute,
    artifactsRoute,
    artifactViewRoute,
    tasksRoute,
    activityRoute,
    runsRoute,
    promptsRoute,
    orgRoute,
    settingsRoute,
    statusRoute,
  ]),
]);

export const router = createRouter({
  routeTree,
  basepath: '/',
  defaultPreload: 'intent',
  parseSearch: parseAppSearch,
  stringifySearch: stringifyAppSearch,
});

declare module '@tanstack/react-router' {
  interface Register {
    router: typeof router;
  }
}
