// @arch arch_MK2Q2.4
import { useCallback, useEffect, useState } from 'react';
import { Link, Outlet, useNavigate, useRouterState } from '@tanstack/react-router';
import {
  BookOpen,
  Boxes,
  ChevronDown,
  ChevronRight,
  Cpu,
  FileCode2,
  FileStack,
  FolderOpen,
  FolderTree,
  GitCommitHorizontal,
  LayoutList,
  ListChecks,
  Monitor,
  Moon,
  Plus,
  Settings,
  Sun,
  type LucideIcon,
} from 'lucide-react';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Toaster } from '@/components/ui/sonner';
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarTrigger,
} from '@/components/ui/sidebar';
import { TooltipProvider } from '@/components/ui/tooltip';
import { useIsMobile } from '@/hooks/use-mobile';
import { useEventStream, useWsStatus } from '@/hooks/useEventStream';
import { useMe } from '@/hooks/useMe';
import { useRefreshBump, useRefreshToken } from '@/hooks/useRefreshBus';
import { useActiveProject } from '@/hooks/useActiveProject';
import { useTabSync } from '@/hooks/useProjectTabs';
import { navPageVisible } from '@/lib/capabilities';
import { useBackendProfiles } from '@/lib/backend';
import { fetchProjects } from '@/lib/api';
import {
  UPDATE_AUTO_CHECK_MS,
  UPDATE_LAST_NOTIFIED_KEY,
  checkAppUpdate,
  savedUpdateChannel,
} from '@/lib/appUpdate';
import { routeSearch } from '@/lib/searchState';
import { THEME_OPTIONS, useTheme, type ThemePreference } from '@/lib/theme';
import { setUnauthorizedHandler } from '@/lib/transport';
import type { DaemonEvent, ViewName, WsConnectionState } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import { ConnectGate } from './ConnectGate';
import { ConnectionBanner } from './ConnectionBanner';
import { RichTextProvider } from '@/lib/richText';

import { RunDock } from './manager/RunDock';
import { RunDockProvider } from '@/lib/runDock';
import { NotificationBell } from './notifications/NotificationBell';
import { ProjectAddDialog } from './ProjectAddDialog';
import { ProjectTabs } from './ProjectTabs';
import { ProjectsManageDialog } from './ProjectsManageDialog';

type ProjectPage =
  | 'decisions'
  | 'architecture'
  | 'tasks'
  | 'glossary'
  | 'artifacts'
  | 'project'
  | 'runs'
  | 'prompts'
  | 'org'
  | 'activity'
  | 'status'
  | 'settings';

type NavItem = {
  page: ProjectPage;
  label: string;
  icon: LucideIcon;
  to:
    | '/projects/$projectId/decisions'
    | '/projects/$projectId/architecture'
    | '/projects/$projectId/tasks'
    | '/projects/$projectId/glossary'
    | '/projects/$projectId/artifacts'
    | '/projects/$projectId/project'
    | '/projects/$projectId/runs'
    | '/projects/$projectId/prompts'
    | '/projects/$projectId/org'
    | '/projects/$projectId/activity'
    | '/projects/$projectId/status'
    | '/projects/$projectId/settings';
};

const PRIMARY: NavItem[] = [
  { page: 'project', label: 'Project', icon: FolderTree, to: '/projects/$projectId/project' },
  { page: 'decisions', label: 'Decisions', icon: GitCommitHorizontal, to: '/projects/$projectId/decisions' },
  { page: 'architecture', label: 'Architecture', icon: Boxes, to: '/projects/$projectId/architecture' },
  { page: 'tasks', label: 'Tasks', icon: ListChecks, to: '/projects/$projectId/tasks' },
  { page: 'glossary', label: 'Glossary', icon: BookOpen, to: '/projects/$projectId/glossary' },
];

const MORE: NavItem[] = [
  { page: 'artifacts', label: 'Artifacts', icon: FileStack, to: '/projects/$projectId/artifacts' },
  { page: 'prompts', label: 'Prompts', icon: FileCode2, to: '/projects/$projectId/prompts' },
  { page: 'activity', label: 'Activity', icon: LayoutList, to: '/projects/$projectId/activity' },
];

const BRAND_WORDMARK = '/brand/orgasmic-wordmark-ink-vector.svg';

function pathParts(pathname: string): string[] {
  return pathname.split('/').filter(Boolean).map(decodeURIComponent);
}

function pageFromPath(pathname: string): string {
  const parts = pathParts(pathname);
  if (parts[0] === 'board') return 'board';
  if (parts[0] === 'projects') return parts[2] ?? 'decisions';
  return 'board';
}

function pathForView(view: ViewName, projectId: string | null) {
  if (view === 'board' || !projectId) return { to: '/board' as const };
  if (view === 'project') {
    return { to: '/projects/$projectId/project' as const, params: { projectId } };
  }
  if (view === 'task' || view === 'tasks') {
    return { to: '/projects/$projectId/tasks' as const, params: { projectId } };
  }
  if (view === 'manager') {
    return { to: '/projects/$projectId/activity' as const, params: { projectId } };
  }
  return {
    to: `/projects/$projectId/${view}` as
      | '/projects/$projectId/decisions'
      | '/projects/$projectId/architecture'
      | '/projects/$projectId/glossary'
      | '/projects/$projectId/artifacts'
      | '/projects/$projectId/runs'
      | '/projects/$projectId/prompts'
      | '/projects/$projectId/org'
      | '/projects/$projectId/activity'
      | '/projects/$projectId/status'
      | '/projects/$projectId/settings',
    params: { projectId },
  };
}

export function AppShell() {
  const wsState = useWsStatus();
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const location = useRouterState({ select: (state) => state.location });
  const pathname = location.pathname;
  const { activeProjectId } = useActiveProject();
  const projectId = activeProjectId;
  const page = pageFromPath(pathname);
  const { activeProfile, updateProfile, testConnection } = useBackendProfiles();
  const { me, can, isMember, onUnauthorized } = useMe();
  const [authError, setAuthError] = useState<string | null>(null);
  const [moreOpen, setMoreOpen] = useState(!isMobile);
  const bumpRefresh = useRefreshBump();
  // A member has no admin token and authenticates by cookie, so the bearer gate
  // must not block them — only prompt when an admin bearer is actually missing.
  const needsToken = !isMember && (!activeProfile.token || Boolean(authError));
  const visiblePrimary = PRIMARY.filter((item) => navPageVisible(me, projectId, item.page));
  const visibleMore = MORE.filter((item) => navPageVisible(me, projectId, item.page));
  const canWatchSessions = can(projectId, 'sessions.watch');

  useTabSync();

  useEffect(() => {
    setMoreOpen(!isMobile);
  }, [isMobile]);

  useEffect(() => {
    setUnauthorizedHandler((err) => {
      // A dead member cookie drops back to the login gate; an admin bearer 401
      // surfaces inline on the bearer gate as before.
      if (onUnauthorized()) return;
      setAuthError(err.message);
    });
    return () => setUnauthorizedHandler(null);
  }, [onUnauthorized]);

  useEffect(() => {
    let cancelled = false;

    async function checkForAppUpdate() {
      try {
        const channel = savedUpdateChannel();
        const update = await checkAppUpdate(channel);
        if (!update || cancelled) return;

        const notificationKey = `${channel}:${update.version}`;
        if (window.localStorage.getItem(UPDATE_LAST_NOTIFIED_KEY) === notificationKey) return;

        window.localStorage.setItem(UPDATE_LAST_NOTIFIED_KEY, notificationKey);
        const action = update.platform === 'android-sideload' ? 'download the APK' : 'install';
        // Lead with what the running build lacks (first release-note line)
        // rather than the bare version delta — concrete beats abstract.
        const firstNote = update.notes?.split('\n').find((line) => line.trim())?.trim();
        toast.success('Update available', {
          description: `${channel}: ${update.currentVersion} -> ${update.version}${firstNote ? ` — ${firstNote}` : ''}. Open Settings to ${action}.`,
        });
      } catch {
        // Release assets may not exist yet, especially before the first nightly publish.
      }
    }

    void checkForAppUpdate();
    const timer = window.setInterval(() => void checkForAppUpdate(), UPDATE_AUTO_CHECK_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, []);

  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        const kind = event.payload.kind;
        if (
          event.topic === 'board' ||
          event.topic === 'task' ||
          event.topic === 'graph' ||
          event.topic === 'daemon' ||
          event.topic === 'artifact' ||
          kind === 'project_parse_error' ||
          kind === 'daemon_started'
        ) {
          bumpRefresh();
        }
      },
      [bumpRefresh],
    ),
  );

  const goView = useCallback(
    (next: ViewName) => {
      const target = pathForView(next, projectId);
      void navigate(target);
    },
    [navigate, projectId],
  );

  const openTask = useCallback(
    (taskId: string) => {
      if (!projectId) return;
      void navigate({
        to: '/projects/$projectId/tasks',
        params: { projectId },
        search: routeSearch((prev) => ({
          ...prev,
          task: taskId,
        })),
      });
    },
    [navigate, projectId],
  );

  return (
    <TooltipProvider>
      <RichTextProvider projectId={projectId} canReadGraph={can(projectId, 'graph.read')}>
      <RunDockProvider>
      <SidebarProvider>
        <Sidebar collapsible="icon">
          <SidebarHeader>
            <Link
              to="/board"
              aria-label="orgasmic board"
              className="flex h-14 items-center justify-center rounded-md px-3 outline-none hover:bg-sidebar-accent focus-visible:ring-2 focus-visible:ring-sidebar-ring group-data-[collapsible=icon]:size-8 group-data-[collapsible=icon]:p-0"
            >
              <img
                src={BRAND_WORDMARK}
                alt="orgasmic"
                className="h-12 max-w-full object-contain group-data-[collapsible=icon]:h-6"
              />
            </Link>
          </SidebarHeader>
          <SidebarContent>
            <SidebarGroup>
              <SidebarGroupLabel>Primary</SidebarGroupLabel>
              <SidebarGroupContent>
                <SidebarMenu>
                  {visiblePrimary.map((item) => (
                    <NavMenuItem
                      key={item.page}
                      item={item}
                      projectId={projectId}
                      activePage={page}
                    />
                  ))}
                </SidebarMenu>
              </SidebarGroupContent>
            </SidebarGroup>
            {visibleMore.length > 0 ? (
              <SidebarGroup>
                <button
                  type="button"
                  className="flex w-full items-center gap-1 px-2 py-1 text-left text-xs font-medium text-sidebar-foreground/70 group-data-[collapsible=icon]:hidden"
                  aria-expanded={moreOpen}
                  onClick={() => setMoreOpen((open) => !open)}
                >
                  {moreOpen ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
                  <span>More</span>
                </button>
                <SidebarGroupContent className={cn(!moreOpen && 'hidden group-data-[collapsible=icon]:block')}>
                  <SidebarMenu>
                    {visibleMore.map((item) => (
                      <NavMenuItem
                        key={item.page}
                        item={item}
                        projectId={projectId}
                        activePage={page}
                      />
                    ))}
                  </SidebarMenu>
                </SidebarGroupContent>
              </SidebarGroup>
            ) : null}
          </SidebarContent>
          <SidebarFooter>
            <ProjectSidebarFooter />
          </SidebarFooter>
        </Sidebar>
        <SidebarInset>
          <header
            className="sticky top-0 z-10 flex items-center gap-2 border-b bg-background/85 backdrop-blur"
            style={{
              // Edge-to-edge on Android: keep the bar clear of the status bar
              // (and a side notch in landscape) while its background still
              // bleeds up under the bar. No-op off-Android (insets resolve 0).
              minHeight: 'calc(3.5rem + var(--safe-top))',
              paddingTop: 'var(--safe-top)',
              paddingLeft: 'calc(0.75rem + var(--safe-left))',
              paddingRight: 'calc(0.75rem + var(--safe-right))',
            }}
          >
            <SidebarTrigger className="shrink-0" />
            <ProjectTabs />
            <div className="ml-auto flex shrink-0 items-center gap-1.5">
              <ConnectionLed state={wsState} onClick={() => goView('status')} />
              {/* The bell polls admin-only parse-error + tx activity; members
                  (who 403 those routes) never mount it. */}
              {!isMember ? (
                <NotificationBell
                  projectId={projectId}
                  onNavigate={goView}
                  onOpenTask={openTask}
                />
              ) : null}
              <ThemeToggle className="hidden md:inline-flex" />
              <Button
                type="button"
                variant="ghost"
                size="icon"
                className="hidden md:inline-flex"
                aria-label="Settings"
                onClick={() => goView('settings')}
              >
                <Settings />
              </Button>
              <MobileOverflow onNavigate={goView} />
            </div>
          </header>
          <ConnectGate
            open={needsToken}
            activeProfile={activeProfile}
            updateProfile={updateProfile}
            testConnection={testConnection}
            onConnected={() => setAuthError(null)}
          />
          <ConnectionBanner wsState={wsState} />
          <div className="flex-1 min-h-0 overflow-auto p-4 pb-24 md:pb-28">
            {/* Hold the routed content until the bearer gate is satisfied.
                Mounting the Outlet under the open ConnectGate fires the route's
                initial fetches with no token yet — they 401, and useResource
                caches that error without re-running once the token lands, so a
                stale "401 missing or invalid bearer token" would persist after a
                first-time connect. */}
            {needsToken ? null : <Outlet />}
          </div>
        </SidebarInset>
        {/* The run dock is an admin/manager surface — it polls admin-only
            manager + runs state, so members never mount it (a member's
            read-only session viewing is a separate, not-yet-exposed surface). */}
        {canWatchSessions && !isMember ? <RunDock /> : null}
        <Toaster position="bottom-right" />
      </SidebarProvider>
      </RunDockProvider>
      </RichTextProvider>
    </TooltipProvider>
  );
}

function NavMenuItem({
  item,
  projectId,
  activePage,
}: {
  item: NavItem;
  projectId: string | null;
  activePage: string;
}) {
  const Icon = item.icon;
  const active = item.page === activePage || (item.page === 'decisions' && activePage === 'projects');

  return (
    <SidebarMenuItem>
      {projectId ? (
        <SidebarMenuButton asChild isActive={active} tooltip={item.label}>
          <Link
            to={item.to}
            params={{ projectId }}
            activeProps={{ className: 'font-medium' }}
          >
            <Icon />
            <span>{item.label}</span>
          </Link>
        </SidebarMenuButton>
      ) : (
        <SidebarMenuButton disabled tooltip={item.label}>
          <Icon />
          <span>{item.label}</span>
        </SidebarMenuButton>
      )}
    </SidebarMenuItem>
  );
}

const LED_LABELS: Record<WsConnectionState, string> = {
  connecting: 'Connecting',
  open: 'Connected',
  reconnecting: 'Reconnecting',
  closed: 'Closed',
};

function ConnectionLed({
  state,
  onClick,
}: {
  state: WsConnectionState;
  onClick: () => void;
}) {
  const dotClass =
    state === 'open'
      ? 'bg-teal-500'
      : state === 'closed'
        ? 'bg-red-500'
        : 'animate-pulse bg-amber-500';

  return (
    <Button
      type="button"
      variant="ghost"
      size="icon"
      aria-label={`Connection: ${LED_LABELS[state]}`}
      title={`Connection: ${LED_LABELS[state]}`}
      onClick={onClick}
    >
      <span className={cn('size-2.5 rounded-full', dotClass)} aria-hidden="true" />
    </Button>
  );
}

const THEME_ICONS: Record<ThemePreference, LucideIcon> = {
  system: Monitor,
  dark: Moon,
  light: Sun,
};

function ThemeToggle({ className }: { className?: string }) {
  const { preference, setPreference } = useTheme();
  const Icon = THEME_ICONS[preference];
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label="Theme" className={className}>
          <Icon />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <ThemeMenuItems onSelectTheme={setPreference} />
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function MobileOverflow({ onNavigate }: { onNavigate: (next: ViewName) => void }) {
  const { setPreference } = useTheme();

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="icon"
          className="md:hidden"
          aria-label="More"
        >
          <ChevronDown />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <ThemeMenuItems onSelectTheme={setPreference} />
        <DropdownMenuItem onClick={() => onNavigate('status')}>
          <Cpu />
          <span>Status</span>
        </DropdownMenuItem>
        <DropdownMenuItem onClick={() => onNavigate('settings')}>
          <Settings />
          <span>Settings</span>
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function ProjectSidebarFooter() {
  const refresh = useRefreshToken();
  const { isMember } = useMe();
  // The admin `/projects` list (and the Add/Manage actions it feeds) are
  // admin-only; a member would 403, so skip the fetch entirely for them.
  const { data } = useResource(`projects:${refresh}:sidebar-footer`, fetchProjects, {
    enabled: !isMember,
  });
  const [addOpen, setAddOpen] = useState(false);
  const [manageOpen, setManageOpen] = useState(false);
  const addLabel = data?.length === 0 ? 'Add your first project' : 'Add project';

  return (
    <>
      <div className="flex flex-col gap-1 group-data-[collapsible=icon]:items-center">
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="w-full justify-start group-data-[collapsible=icon]:size-8 group-data-[collapsible=icon]:p-0"
          aria-label={addLabel}
          onClick={() => setAddOpen(true)}
        >
          <Plus />
          <span className="truncate group-data-[collapsible=icon]:hidden">{addLabel}</span>
        </Button>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="w-full justify-start group-data-[collapsible=icon]:size-8 group-data-[collapsible=icon]:p-0"
          aria-label="Manage projects"
          onClick={() => setManageOpen(true)}
        >
          <FolderOpen />
          <span className="truncate group-data-[collapsible=icon]:hidden">Manage projects</span>
        </Button>
      </div>
      <ProjectAddDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onOpenManage={() => {
          setAddOpen(false);
          setManageOpen(true);
        }}
      />
      <ProjectsManageDialog open={manageOpen} onOpenChange={setManageOpen} />
    </>
  );
}

function ThemeMenuItems({ onSelectTheme }: { onSelectTheme: (next: ThemePreference) => void }) {
  return (
    <>
      {THEME_OPTIONS.map((option) => {
        const Icon = THEME_ICONS[option.value];
        return (
          <DropdownMenuItem key={option.value} onClick={() => onSelectTheme(option.value)}>
            <Icon />
            <span>{option.label}</span>
          </DropdownMenuItem>
        );
      })}
    </>
  );
}
