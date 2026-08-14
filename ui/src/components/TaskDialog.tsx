import { useEffect, useMemo, useRef, useState } from 'react';
import {
  Bot,
  CircleUserRound,
  Eye,
  ListTree,
  Loader2,
  MessageSquare,
  MessageSquarePlus,
  PanelLeftClose,
  PanelLeftOpen,
  PanelRightClose,
  PanelRightOpen,
  Pencil,
  Server,
  User,
} from 'lucide-react';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogTitle, DialogDescription } from '@/components/ui/dialog';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Separator } from '@/components/ui/separator';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Skeleton } from '@/components/ui/skeleton';
import { Textarea } from '@/components/ui/textarea';
import { ManagerChatTranscript } from '@/components/manager/ManagerChatTranscript';
import { NestedTreeRow } from '@/components/NestedTreeRow';
import { PeekBackButton } from '@/components/PeekBackButton';
import { TaskAgentBadges } from '@/components/TaskAgentBadges';
import { useIsMobile } from '@/hooks/use-mobile';
import { useTaskRuns } from '@/hooks/useTaskRuns';
import { NodeDocEditor, type NodeDirectory } from '@/components/orgdoc/NodeDocEditor';
import { TASK_DESCRIPTOR } from '@/components/orgdoc/descriptor';
import { useMe } from '@/hooks/useMe';
import { useRefreshBump, useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchProject, fetchTask, fetchTaskActivity, postTaskComment } from '@/lib/api';
import type {
  ActivityEntry,
  LifecycleStage,
  TaskDetail,
  TaskSummary,
} from '@/lib/types';
import { lifecycleStageLabel } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { decorateText, useRichText } from '@/lib/richText';
import { cn } from '@/lib/utils';

type BadgeVariant = 'default' | 'secondary' | 'destructive' | 'outline';

function stageVariant(stage: LifecycleStage | string): BadgeVariant {
  switch (stage) {
    case 'done':
      return 'secondary';
    case 'in_progress':
    case 'in_review':
      return 'default';
    case 'cancelled':
      return 'destructive';
    default:
      return 'outline';
  }
}

function shortRunId(runId: string): string {
  if (runId.length <= 12) return runId;
  return `${runId.slice(0, 8)}…${runId.slice(-4)}`;
}

function buildTaskDirectory(tasks: TaskSummary[]): NodeDirectory {
  return {
    labelFor: (id) => tasks.find((task) => task.id === id)?.title ?? id,
    suggestionsFor: (source) => {
      if (source !== 'task') return [];
      return tasks.map((task) => ({ value: task.id, label: task.title }));
    },
  };
}

export function TaskDialog({
  projectId,
  taskId,
  historyDepth,
  onBack,
  onClose,
  onSelectTask,
}: {
  projectId: string | null;
  taskId: string | null;
  historyDepth: number;
  onBack: () => void;
  onClose: () => void;
  onSelectTask: (taskId: string) => void;
}) {
  const open = Boolean(projectId && taskId);
  return (
    <Dialog open={open} onOpenChange={(next) => !next && onClose()}>
      <DialogContent
        showCloseButton
        className="grid h-[min(92dvh,54rem)] w-[min(96vw,90rem)] max-w-none grid-rows-[auto_auto_1fr] gap-0 overflow-hidden p-0 sm:max-w-none"
      >
        {projectId && taskId ? (
          <TaskDialogBody
            projectId={projectId}
            taskId={taskId}
            historyDepth={historyDepth}
            onBack={onBack}
            onSelectTask={onSelectTask}
            onClose={onClose}
          />
        ) : (
          <DialogTitle className="sr-only">Task</DialogTitle>
        )}
      </DialogContent>
    </Dialog>
  );
}

function TaskDialogBody({
  projectId,
  taskId,
  historyDepth,
  onBack,
  onSelectTask,
  onClose,
}: {
  projectId: string;
  taskId: string;
  historyDepth: number;
  onBack: () => void;
  onSelectTask: (id: string) => void;
  onClose: () => void;
}) {
  const [mode, setMode] = useState<'view' | 'edit'>('view');
  const isMobile = useIsMobile();
  const [isWide, setIsWide] = useState(() =>
    typeof window === 'undefined' ? true : window.innerWidth >= 1280,
  );
  const [hierarchyOpen, setHierarchyOpen] = useState(() =>
    typeof window === 'undefined' ? true : window.innerWidth >= 1280,
  );
  const [activityOpen, setActivityOpen] = useState(() =>
    typeof window === 'undefined' ? true : window.innerWidth >= 768,
  );
  const { can } = useMe();
  const refresh = useRefreshToken();
  const bumpRefresh = useRefreshBump();
  const canComment = can(projectId, 'tasks.comment');
  const task = useResource(
    `task-dialog:${projectId}:${taskId}:${refresh}`,
    () => fetchTask(projectId, taskId),
  );
  const project = useResource(
    `task-dialog-project:${projectId}:${refresh}`,
    () => fetchProject(projectId),
  );
  const activity = useResource(
    `task-dialog-activity:${taskId}:${refresh}`,
    () => fetchTaskActivity(projectId, taskId),
  );

  const subtasks = useMemo(() => {
    const all = project.data?.tasks ?? [];
    return all.filter((t) => t.parent_task === taskId);
  }, [project.data, taskId]);

  const parent = useMemo(() => {
    const pid = task.data?.parent_task;
    if (!pid) return null;
    return (project.data?.tasks ?? []).find((t) => t.id === pid) ?? null;
  }, [task.data, project.data]);
  const taskDirectory = useMemo(
    () => buildTaskDirectory(project.data?.tasks ?? []),
    [project.data],
  );
  useEffect(() => {
    setMode('view');
  }, [taskId]);
  useEffect(() => {
    const media = window.matchMedia('(min-width: 1280px)');
    const onChange = () => setIsWide(media.matches);
    media.addEventListener('change', onChange);
    onChange();
    return () => media.removeEventListener('change', onChange);
  }, []);
  useEffect(() => {
    if (isMobile) {
      setHierarchyOpen(false);
      setActivityOpen(false);
    }
  }, [isMobile]);
  useEffect(() => {
    if (!isMobile && !isWide && hierarchyOpen && activityOpen) {
      setHierarchyOpen(false);
    }
  }, [activityOpen, hierarchyOpen, isMobile, isWide]);

  function toggleHierarchy() {
    const next = !hierarchyOpen;
    setHierarchyOpen(next);
    if (next && !isWide) setActivityOpen(false);
  }

  function toggleActivity() {
    const next = !activityOpen;
    setActivityOpen(next);
    if (next && !isWide) setHierarchyOpen(false);
  }

  const dialogDescription = task.data
    ? `Task ${taskId} details: ${task.data.title}`
    : `Task ${taskId} details`;

  return (
    <>
      <DialogDescription className="sr-only">{dialogDescription}</DialogDescription>
      <DialogHeader
        task={task.data}
        taskId={taskId}
        historyDepth={historyDepth}
        loading={task.loading && !task.data}
        mode={mode}
        onBack={onBack}
        onToggleMode={() => setMode((current) => (current === 'edit' ? 'view' : 'edit'))}
        onClose={onClose}
      />
      <PaneToolbar
        hierarchyOpen={hierarchyOpen}
        activityOpen={activityOpen}
        activityCount={activity.data?.length ?? 0}
        onToggleHierarchy={toggleHierarchy}
        onToggleActivity={toggleActivity}
      />
      <div
        className={cn(
          'grid min-h-0 grid-cols-1 overflow-hidden',
          hierarchyOpen && activityOpen && 'md:grid-cols-[15rem_minmax(0,1fr)_20rem]',
          hierarchyOpen && !activityOpen && 'md:grid-cols-[15rem_minmax(0,1fr)]',
          !hierarchyOpen && activityOpen && 'md:grid-cols-[minmax(0,1fr)_20rem]',
        )}
      >
        {hierarchyOpen && !isMobile ? (
          <SubtaskRail
            parent={parent}
            subtasks={subtasks}
            loading={project.loading && !project.data}
            onSelectTask={onSelectTask}
          />
        ) : null}
        <MainPane
          projectId={projectId}
          task={task.data}
          loading={task.loading && !task.data}
          mode={mode}
          directory={taskDirectory}
          onSelectTask={onSelectTask}
        />
        {activityOpen && !isMobile ? (
          <TaskActivityRail
            projectId={projectId}
            taskId={taskId}
            entries={activity.data ?? []}
            loading={activity.loading && !activity.data}
            canComment={canComment}
            onChanged={bumpRefresh}
          />
        ) : null}
      </div>
      {isMobile ? (
        <>
          <Sheet open={hierarchyOpen} onOpenChange={setHierarchyOpen}>
            <SheetContent side="left" className="w-[min(92vw,22rem)] gap-0 p-0">
              <SheetHeader className="sr-only">
                <SheetTitle>Task hierarchy</SheetTitle>
                <SheetDescription>Parent task and subtasks for {taskId}.</SheetDescription>
              </SheetHeader>
              <SubtaskRail
                parent={parent}
                subtasks={subtasks}
                loading={project.loading && !project.data}
                onSelectTask={onSelectTask}
                embedded
              />
            </SheetContent>
          </Sheet>
          <Sheet open={activityOpen} onOpenChange={setActivityOpen}>
            <SheetContent side="right" className="w-[min(92vw,24rem)] gap-0 p-0">
              <SheetHeader className="sr-only">
                <SheetTitle>Task activity</SheetTitle>
                <SheetDescription>Activity history for {taskId}.</SheetDescription>
              </SheetHeader>
              <TaskActivityRail
                projectId={projectId}
                taskId={taskId}
                entries={activity.data ?? []}
                loading={activity.loading && !activity.data}
                canComment={canComment}
                onChanged={bumpRefresh}
                embedded
              />
            </SheetContent>
          </Sheet>
        </>
      ) : null}
    </>
  );
}

function PaneToolbar({
  hierarchyOpen,
  activityOpen,
  activityCount,
  onToggleHierarchy,
  onToggleActivity,
}: {
  hierarchyOpen: boolean;
  activityOpen: boolean;
  activityCount: number;
  onToggleHierarchy: () => void;
  onToggleActivity: () => void;
}) {
  return (
    <div className="flex h-11 shrink-0 items-center gap-2 border-b bg-muted/10 px-2 sm:px-3">
      <Button
        type="button"
        variant={hierarchyOpen ? 'secondary' : 'ghost'}
        size="sm"
        aria-expanded={hierarchyOpen}
        aria-controls="task-hierarchy-panel"
        aria-label={hierarchyOpen ? 'Close hierarchy' : 'Open hierarchy'}
        onClick={onToggleHierarchy}
      >
        {hierarchyOpen ? <PanelLeftClose /> : <PanelLeftOpen />}
        <span className="hidden sm:inline">Hierarchy</span>
      </Button>
      <span className="min-w-0 flex-1 truncate text-center text-xs font-medium text-muted-foreground">
        Task details
      </span>
      <Button
        type="button"
        variant={activityOpen ? 'secondary' : 'ghost'}
        size="sm"
        aria-expanded={activityOpen}
        aria-controls="task-activity-panel"
        aria-label={activityOpen ? 'Close activity' : 'Open activity'}
        onClick={onToggleActivity}
      >
        {activityOpen ? <PanelRightClose /> : <PanelRightOpen />}
        <span className="hidden sm:inline">Activity</span>
        <span className="font-mono text-[10px] text-muted-foreground">{activityCount}</span>
      </Button>
    </div>
  );
}

function DialogHeader({
  task,
  taskId,
  historyDepth,
  loading,
  mode,
  onBack,
  onToggleMode,
  onClose,
}: {
  task: TaskSummary | null;
  taskId: string;
  historyDepth: number;
  loading: boolean;
  mode: 'view' | 'edit';
  onBack: () => void;
  onToggleMode: () => void;
  onClose: () => void;
}) {
  const [chatOpen, setChatOpen] = useState(false);
  const ownerButtonRef = useRef<HTMLButtonElement | null>(null);
  const taskRuns = useTaskRuns();
  const match = taskRuns.forTask(task?.id ?? '');
  const hasLiveRun = match.running.length > 0;

  if (loading) {
    return (
      <div className="flex items-start gap-3 border-b px-5 py-4 pr-12">
        <DialogTitle className="sr-only">Task {taskId}</DialogTitle>
        <PeekBackButton depth={historyDepth} onBack={onBack} />
        <div className="flex min-w-0 flex-1 flex-col gap-2">
          <Skeleton className="h-4 w-32" />
          <Skeleton className="h-6 w-3/4" />
        </div>
      </div>
    );
  }
  if (!task) {
    return (
      <div className="flex items-center gap-3 border-b px-5 py-4 pr-12">
        <DialogTitle className="sr-only">Task {taskId}</DialogTitle>
        <PeekBackButton depth={historyDepth} onBack={onBack} />
        <span className="text-sm text-muted-foreground">Task details unavailable</span>
      </div>
    );
  }
  const canOpenAgentChat = !hasLiveRun && task.owner.startsWith('agent.') && task.run_id != null;
  const runId = task.run_id ?? '';
  const runSlug = runId ? shortRunId(runId) : '';

  const handleChatOpenChange = (nextOpen: boolean) => {
    setChatOpen(nextOpen);
    if (!nextOpen) {
      window.setTimeout(() => ownerButtonRef.current?.focus(), 0);
    }
  };

  return (
    <div className="flex flex-wrap items-start gap-3 border-b px-5 py-4 pr-12 sm:flex-nowrap">
      <PeekBackButton depth={historyDepth} onBack={onBack} />
      <div className="order-3 min-w-0 w-full sm:order-none sm:w-auto sm:flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <code className="font-mono text-xs text-muted-foreground">{task.id}</code>
          <Separator orientation="vertical" className="h-4" />
          <Badge variant={stageVariant(task.lifecycle_stage)} className="capitalize">
            {lifecycleStageLabel(task.lifecycle_stage)}
          </Badge>
          {task.priority ? (
            <Badge variant="secondary" className="font-mono text-[10px]">
              {task.priority}
            </Badge>
          ) : null}
          {!hasLiveRun && task.owner ? (
            canOpenAgentChat ? (
              <>
                <button
                  ref={ownerButtonRef}
                  type="button"
                  aria-expanded={chatOpen}
                  aria-label={`View live agent chat for ${task.id}`}
                  onClick={() => setChatOpen(true)}
                  className="group rounded-4xl focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
                >
                  <Badge
                    variant="default"
                    className="cursor-pointer gap-1 font-mono text-[10px] transition hover:ring-1 hover:ring-ring/40 group-hover:bg-primary/90"
                  >
                    <User className="size-2.5" />
                    {task.owner}
                  </Badge>
                </button>
                <Sheet open={chatOpen} onOpenChange={handleChatOpenChange}>
                  <SheetContent side="right" className="w-[min(92vw,34rem)] gap-0 p-0 sm:max-w-lg">
                    <SheetHeader className="border-b pr-12">
                      <SheetTitle className="flex items-center gap-2 font-mono text-sm">
                        <MessageSquare className="size-4 text-muted-foreground" />
                        <span>
                          {task.id} · {runSlug}
                        </span>
                      </SheetTitle>
                      <SheetDescription>
                        Live transcript for {task.owner}.
                      </SheetDescription>
                    </SheetHeader>
                    <div className="min-h-0 flex-1">
                      <ManagerChatTranscript runId={runId} />
                    </div>
                  </SheetContent>
                </Sheet>
              </>
            ) : (
              <Badge
                variant={task.owner === 'human' ? 'outline' : 'default'}
                className="gap-1 font-mono text-[10px]"
              >
                <User className="size-2.5" />
                {task.owner}
              </Badge>
            )
          ) : null}
          <TaskAgentBadges match={match} onOpen={onClose} />
        </div>
        <DialogTitle className="mt-2 text-balance text-base font-semibold leading-snug sm:text-lg">
          {task.title}
        </DialogTitle>
      </div>
      <Button
        type="button"
        variant={mode === 'edit' ? 'default' : 'outline'}
        size="sm"
        className="order-2 ml-auto shrink-0 sm:order-none sm:ml-0"
        onClick={onToggleMode}
        aria-pressed={mode === 'edit'}
      >
        {mode === 'edit' ? <Eye /> : <Pencil />}
        {mode === 'edit' ? 'View' : 'Edit'}
      </Button>
    </div>
  );
}

function SubtaskRail({
  parent,
  subtasks,
  loading,
  onSelectTask,
  embedded = false,
}: {
  parent: TaskSummary | null;
  subtasks: TaskSummary[];
  loading: boolean;
  onSelectTask: (id: string) => void;
  embedded?: boolean;
}) {
  return (
    <aside
      id="task-hierarchy-panel"
      aria-label="Task hierarchy"
      className={cn(
        'min-h-0 flex-col overflow-hidden bg-muted/20',
        embedded ? 'flex h-full' : 'hidden border-r md:flex',
      )}
    >
      <div className={cn('flex shrink-0 items-center gap-2 border-b px-4 py-3', embedded && 'pr-12')}>
        <ListTree className="size-3.5 text-muted-foreground" />
        <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Hierarchy
        </span>
      </div>
      <ScrollArea
        className="min-h-0 min-w-0 flex-1"
        viewportClassName="overflow-x-hidden [&>div]:!block [&>div]:min-w-0"
      >
        <div className="flex flex-col gap-3 px-2 py-3">
          {parent ? (
            <Section label="Parent">
              <TaskRailRow task={parent} onClick={() => onSelectTask(parent.id)} />
            </Section>
          ) : null}
          <Section label={`Subtasks${subtasks.length ? ` (${subtasks.length})` : ''}`}>
            {loading ? (
              <Skeleton className="h-8" />
            ) : subtasks.length === 0 ? (
              <p className="rounded-md border border-dashed bg-background/40 px-3 py-2 text-xs text-muted-foreground">
                No subtasks.
              </p>
            ) : (
              <ul className="flex flex-col gap-1">
                {subtasks.map((s) => (
                  <li key={s.id}>
                    <TaskRailRow task={s} depth={1} onClick={() => onSelectTask(s.id)} />
                  </li>
                ))}
              </ul>
            )}
          </Section>
        </div>
      </ScrollArea>
    </aside>
  );
}

function Section({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-1.5">
      <span className="px-1 text-[10px] uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
      {children}
    </div>
  );
}

function TaskRailRow({
  task,
  depth = 0,
  onClick,
}: {
  task: TaskSummary;
  depth?: number;
  onClick: () => void;
}) {
  return (
    <NestedTreeRow
      depth={depth}
      nodeId={task.id}
      nodeKind="task"
      title={task.title}
      indent="compact"
      titleLines={2}
      onOpen={onClick}
      openLabel={`Open ${task.id}: ${task.title}`}
      className="rounded-md"
      meta={
        <Badge variant={stageVariant(task.lifecycle_stage)} className="text-[10px] capitalize">
          {lifecycleStageLabel(task.lifecycle_stage)}
        </Badge>
      }
    />
  );
}

function MainPane({
  projectId,
  task,
  loading,
  mode,
  directory,
  onSelectTask,
}: {
  projectId: string;
  task: TaskDetail | null;
  loading: boolean;
  mode: 'view' | 'edit';
  directory: NodeDirectory;
  onSelectTask: (taskId: string) => void;
}) {
  if (loading) {
    return (
      <div className="flex flex-col gap-4 px-5 py-5">
        <Skeleton className="h-4 w-40" />
        <Skeleton className="h-24" />
      </div>
    );
  }
  if (!task) return null;
  return (
    <ScrollArea className="min-h-0">
      <div className="mx-auto w-full max-w-[75ch] px-5 py-6 [overflow-wrap:anywhere] sm:px-7 lg:px-8">
        <NodeDocEditor
          projectId={projectId}
          nodeId={task.id}
          descriptor={TASK_DESCRIPTOR}
          directory={directory}
          onOpenNode={onSelectTask}
          mode={mode}
          apiKind="task"
        />
        <Separator className="my-5" />
        <p className="text-xs text-muted-foreground">
          Source: <code className="font-mono">{shortPath(task.source_file)}</code>
        </p>
      </div>
    </ScrollArea>
  );
}
export function TaskActivityRail({
  projectId,
  taskId,
  entries,
  loading,
  canComment,
  onChanged,
  embedded = false,
}: {
  projectId: string;
  taskId: string;
  entries: ActivityEntry[];
  loading: boolean;
  canComment: boolean;
  onChanged: () => void;
  embedded?: boolean;
}) {
  return (
    <aside
      id="task-activity-panel"
      aria-label="Task activity"
      className={cn(
        'min-h-0 flex-col overflow-hidden bg-muted/20',
        embedded ? 'flex h-full' : 'hidden border-l md:flex',
      )}
    >
      <div className={cn('flex shrink-0 items-center gap-2 border-b px-4 py-3', embedded && 'pr-12')}>
        <MessageSquare className="size-3.5 text-muted-foreground" />
        <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Activity
        </span>
        <span className="ml-auto text-[10px] font-mono text-muted-foreground">
          {entries.length}
        </span>
      </div>
      <ScrollArea
        className="min-h-0 min-w-0 flex-1"
        viewportClassName="overflow-x-hidden [&>div]:!block [&>div]:min-w-0"
      >
        <div className="w-full min-w-0 max-w-full overflow-x-hidden px-4 py-3">
          {loading ? (
            <Skeleton className="h-20" />
          ) : entries.length === 0 ? (
            <p className="rounded-md border border-dashed bg-background/40 px-3 py-2 text-xs text-muted-foreground">
              {canComment ? 'No activity yet. Start the conversation below.' : 'No activity yet.'}
            </p>
          ) : (
            <div className="flex flex-col gap-2.5">
              {entries.map((e) => <ActivityRow key={e.tx_id} entry={e} />)}
            </div>
          )}
        </div>
      </ScrollArea>
      {canComment ? (
        <TaskCommentComposer
          projectId={projectId}
          taskId={taskId}
          onChanged={onChanged}
        />
      ) : null}
    </aside>
  );
}

export type TaskActivityPresentation = {
  source: 'human' | 'agent' | 'daemon';
  sourceLabel: string;
  eventLabel: string;
  body: string;
};

export function taskActivityPresentation(entry: ActivityEntry): TaskActivityPresentation {
  const isAgent = entry.actor.startsWith('agent.');
  if (entry.kind === 'comment' && !isAgent) {
    return {
      source: 'human',
      sourceLabel: entry.actor || 'Team member',
      eventLabel: 'Team comment',
      body: entry.body,
    };
  }

  if (isAgent) {
    const role = entry.actor
      .slice('agent.'.length)
      .replace(/[._-]+/g, ' ')
      .trim();
    const sourceLabel = role
      ? `${role.charAt(0).toUpperCase()}${role.slice(1)} agent`
      : 'Agent';
    return {
      source: 'agent',
      sourceLabel,
      eventLabel: entry.kind === 'comment' ? 'Agent update' : 'Run event',
      body: readableActivityBody(entry),
    };
  }

  return {
    source: 'daemon',
    sourceLabel: 'Orgasmic daemon',
    eventLabel: entry.kind === 'state_transition' ? 'Status change' : 'Run event',
    body: readableActivityBody(entry),
  };
}

function ActivityRow({ entry }: { entry: ActivityEntry }) {
  const rich = useRichText();
  const presentation = taskActivityPresentation(entry);
  const automated = presentation.source !== 'human';
  const SourceIcon = presentation.source === 'agent' ? Bot : Server;

  if (!automated) {
    return (
      <article
        className="min-w-0 rounded-lg border bg-background/70 px-3 py-2.5"
        aria-label={`Comment from ${presentation.sourceLabel}`}
      >
        <div className="flex min-w-0 items-center gap-2">
          <span className="flex size-6 shrink-0 items-center justify-center rounded-full bg-accent text-accent-foreground">
            <CircleUserRound className="size-3.5" aria-hidden="true" />
          </span>
          <span className="min-w-0 flex-1 truncate text-xs font-semibold">
            {presentation.sourceLabel}
          </span>
          <time
            className="shrink-0 font-mono text-[9px] text-muted-foreground"
            title={entry.time ?? undefined}
          >
            {shortTime(entry.time)}
          </time>
        </div>
        <p className="mt-2 whitespace-pre-wrap break-words text-[12px] leading-relaxed">
          {decorateText(presentation.body, rich)}
        </p>
        <ActivityArtifacts artifacts={entry.artifacts} />
      </article>
    );
  }

  return (
    <article className="grid min-w-0 grid-cols-[1.75rem_minmax(0,1fr)] gap-2.5 py-1.5">
      <span
        className={cn(
          'mt-0.5 flex size-7 items-center justify-center rounded-md',
          presentation.source === 'agent'
            ? 'bg-primary/10 text-primary'
            : 'bg-muted text-muted-foreground',
        )}
      >
        <SourceIcon className="size-3.5" aria-hidden="true" />
      </span>
      <div className="min-w-0">
        <div className="flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5">
          <span className="min-w-0 truncate text-[11px] font-semibold">
            {presentation.sourceLabel}
          </span>
          <span className="text-[9px] text-muted-foreground">{presentation.eventLabel}</span>
          <time
            className="ml-auto shrink-0 font-mono text-[9px] text-muted-foreground"
            title={entry.time ?? undefined}
          >
            {shortTime(entry.time)}
          </time>
        </div>
        <p className="mt-1 whitespace-pre-wrap break-words text-[11px] leading-snug text-muted-foreground">
          {decorateText(presentation.body, rich)}
        </p>
        <ActivityArtifacts artifacts={entry.artifacts} />
      </div>
    </article>
  );
}

function ActivityArtifacts({ artifacts }: { artifacts: string[] }) {
  if (!artifacts || artifacts.length === 0) return null;
  return (
    <div className="mt-2 flex flex-wrap gap-1">
      {artifacts.slice(0, 6).map((artifact) => (
        <Badge key={artifact} variant="outline" className="font-mono text-[9px]">
          {artifact}
        </Badge>
      ))}
      {artifacts.length > 6 ? (
        <Badge variant="secondary" className="font-mono text-[9px]">
          +{artifacts.length - 6}
        </Badge>
      ) : null}
    </div>
  );
}

function TaskCommentComposer({
  projectId,
  taskId,
  onChanged,
}: {
  projectId: string;
  taskId: string;
  onChanged: () => void;
}) {
  const [message, setMessage] = useState('');
  const [posting, setPosting] = useState(false);

  async function submitComment() {
    const body = message.trim();
    if (!body || posting) return;
    setPosting(true);
    try {
      await postTaskComment(projectId, taskId, { body });
      setMessage('');
      onChanged();
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    } finally {
      setPosting(false);
    }
  }

  return (
    <form
      aria-label="Add task comment"
      className="shrink-0 border-t bg-background/80 p-3"
      onSubmit={(event) => {
        event.preventDefault();
        void submitComment();
      }}
    >
      <div className="mb-2 flex items-center gap-2">
        <MessageSquarePlus className="size-3.5 text-muted-foreground" aria-hidden="true" />
        <label htmlFor={`task-comment-${taskId}`} className="text-xs font-medium">
          Add a comment
        </label>
      </div>
      <Textarea
        id={`task-comment-${taskId}`}
        rows={3}
        value={message}
        disabled={posting}
        placeholder="Write a team comment…"
        aria-describedby={`task-comment-hint-${taskId}`}
        className="min-h-20 resize-y bg-background text-sm"
        onChange={(event) => setMessage(event.target.value)}
      />
      <div className="mt-2 flex items-center justify-between gap-3">
        <span
          id={`task-comment-hint-${taskId}`}
          className="text-[10px] leading-snug text-muted-foreground"
        >
          Shared with the team and the next agent pickup.
        </span>
        <Button type="submit" size="sm" disabled={!message.trim() || posting}>
          {posting ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
          {posting ? 'Posting…' : 'Comment'}
        </Button>
      </div>
    </form>
  );
}

function readableActivityBody(entry: ActivityEntry): string {
  if (entry.kind !== 'state_transition') return entry.body;
  const transition = /^transition\s+(\S+)\s+to\s+(\S+)$/i.exec(entry.body.trim());
  if (!transition) return entry.body;
  const state = transition[2].replaceAll('_', ' ');
  const stateLabel = state.charAt(0).toUpperCase() + state.slice(1);
  return `${transition[1]} moved to ${stateLabel}`;
}

function shortPath(p: string | null | undefined): string {
  if (!p) return '—';
  const idx = p.lastIndexOf('/');
  return idx >= 0 ? p.slice(idx + 1) : p;
}

function shortTime(t: string | null | undefined): string {
  if (!t) return '';
  // Org timestamp: [YYYY-MM-DD Day HH:MM:SS]
  const m = /\[(\d{4}-\d{2}-\d{2})[^\]]*?(\d{2}:\d{2})/.exec(t);
  if (m) return `${m[1]} ${m[2]}`;
  return t;
}
