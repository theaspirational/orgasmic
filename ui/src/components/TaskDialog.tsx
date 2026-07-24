// @arch arch_MK2Q2.6
import { useEffect, useMemo, useRef, useState } from 'react';
import {
  Eye,
  ListTree,
  MessageSquare,
  Pencil,
  User,
} from 'lucide-react';

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
import { ManagerChatTranscript } from '@/components/manager/ManagerChatTranscript';
import { TaskAgentBadges } from '@/components/TaskAgentBadges';
import { useTaskRuns } from '@/hooks/useTaskRuns';
import { NodeDocEditor, type NodeDirectory } from '@/components/orgdoc/NodeDocEditor';
import { TASK_DESCRIPTOR } from '@/components/orgdoc/descriptor';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchProject, fetchTask, fetchTaskActivity } from '@/lib/api';
import type {
  ActivityEntry,
  LifecycleStage,
  TaskDetail,
  TaskSummary,
} from '@/lib/types';
import { lifecycleStageLabel } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { decorateText, useRichText } from '@/lib/richText';

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
  onClose,
  onSelectTask,
}: {
  projectId: string | null;
  taskId: string | null;
  onClose: () => void;
  onSelectTask: (taskId: string) => void;
}) {
  const open = Boolean(projectId && taskId);
  return (
    <Dialog open={open} onOpenChange={(next) => !next && onClose()}>
      <DialogContent
        showCloseButton
        className="grid h-[min(90vh,46rem)] w-[min(96vw,80rem)] max-w-none grid-rows-[auto_1fr] gap-0 overflow-hidden p-0 sm:max-w-none"
      >
        {projectId && taskId ? (
          <TaskDialogBody
            projectId={projectId}
            taskId={taskId}
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
  onSelectTask,
  onClose,
}: {
  projectId: string;
  taskId: string;
  onSelectTask: (id: string) => void;
  onClose: () => void;
}) {
  const [mode, setMode] = useState<'view' | 'edit'>('view');
  const refresh = useRefreshToken();
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
    () => fetchTaskActivity(taskId),
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
  const dialogDescription = task.data
    ? `Task ${taskId} details: ${task.data.title}`
    : `Task ${taskId} details`;

  return (
    <>
      <DialogDescription className="sr-only">{dialogDescription}</DialogDescription>
      <DialogHeader
        task={task.data}
        taskId={taskId}
        loading={task.loading && !task.data}
        mode={mode}
        onToggleMode={() => setMode((current) => (current === 'edit' ? 'view' : 'edit'))}
        onClose={onClose}
      />
      <div className="grid min-h-0 grid-cols-1 md:grid-cols-[16rem_minmax(0,1fr)_20rem]">
        <SubtaskRail
          parent={parent}
          subtasks={subtasks}
          loading={project.loading && !project.data}
          onSelectTask={onSelectTask}
        />
        <MainPane
          projectId={projectId}
          task={task.data}
          loading={task.loading && !task.data}
          mode={mode}
          directory={taskDirectory}
          onSelectTask={onSelectTask}
        />
        <ActivityRail
          entries={activity.data ?? []}
          loading={activity.loading && !activity.data}
        />
      </div>
    </>
  );
}

function DialogHeader({
  task,
  taskId,
  loading,
  mode,
  onToggleMode,
  onClose,
}: {
  task: TaskSummary | null;
  taskId: string;
  loading: boolean;
  mode: 'view' | 'edit';
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
      <div className="flex flex-col gap-2 border-b px-5 py-4">
        <DialogTitle className="sr-only">Task {taskId}</DialogTitle>
        <Skeleton className="h-4 w-32" />
        <Skeleton className="h-6 w-3/4" />
      </div>
    );
  }
  if (!task) return <DialogTitle className="sr-only">Task {taskId}</DialogTitle>;
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
    <div className="flex items-start gap-3 border-b px-5 py-4 pr-12">
      <div className="min-w-0 flex-1">
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
        className="shrink-0"
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
}: {
  parent: TaskSummary | null;
  subtasks: TaskSummary[];
  loading: boolean;
  onSelectTask: (id: string) => void;
}) {
  return (
    <div className="hidden flex-col border-r bg-muted/20 md:flex">
      <div className="flex items-center gap-2 border-b px-4 py-3">
        <ListTree className="size-3.5 text-muted-foreground" />
        <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Hierarchy
        </span>
      </div>
      <ScrollArea className="flex-1">
        <div className="flex flex-col gap-3 px-3 py-3">
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
                    <TaskRailRow task={s} onClick={() => onSelectTask(s.id)} />
                  </li>
                ))}
              </ul>
            )}
          </Section>
        </div>
      </ScrollArea>
    </div>
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

function TaskRailRow({ task, onClick }: { task: TaskSummary; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="group flex w-full flex-col gap-1 rounded-md border bg-background px-2.5 py-2 text-left transition-colors hover:border-primary/60 focus-visible:border-primary/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
    >
      <div className="flex items-center justify-between gap-2">
        <code className="font-mono text-[10px] text-muted-foreground">{task.id}</code>
        <Badge variant={stageVariant(task.lifecycle_stage)} className="text-[10px] capitalize">
          {lifecycleStageLabel(task.lifecycle_stage)}
        </Badge>
      </div>
      <p className="line-clamp-2 text-[12px] leading-snug">{task.title}</p>
    </button>
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
      <div className="px-5 py-5 [overflow-wrap:anywhere]">
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
function ActivityRail({
  entries,
  loading,
}: {
  entries: ActivityEntry[];
  loading: boolean;
}) {
  return (
    <div className="flex flex-col border-t bg-muted/20 md:border-l md:border-t-0">
      <div className="flex items-center gap-2 border-b px-4 py-3">
        <MessageSquare className="size-3.5 text-muted-foreground" />
        <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Activity
        </span>
        <span className="ml-auto text-[10px] font-mono text-muted-foreground">
          {entries.length}
        </span>
      </div>
      <ScrollArea className="flex-1">
        <div className="flex flex-col gap-3 px-3 py-3">
          {loading ? (
            <Skeleton className="h-20" />
          ) : entries.length === 0 ? (
            <p className="rounded-md border border-dashed bg-background/40 px-3 py-2 text-xs text-muted-foreground">
              No activity yet.
            </p>
          ) : (
            entries.map((e) => <ActivityRow key={e.tx_id} entry={e} />)
          )}
        </div>
      </ScrollArea>
    </div>
  );
}

function ActivityRow({ entry }: { entry: ActivityEntry }) {
  const rich = useRichText();
  const synthesized = entry.actor.startsWith('agent.');
  return (
    <article className="flex flex-col gap-1.5 rounded-md border bg-background px-3 py-2.5">
      <div className="flex items-center justify-between gap-2">
        <code className="truncate font-mono text-[10px] text-muted-foreground">
          {entry.actor}
        </code>
        <span className="shrink-0 font-mono text-[10px] text-muted-foreground/70">
          {shortTime(entry.time)}
        </span>
      </div>
      <p className="whitespace-pre-wrap text-[12px] leading-relaxed">{decorateText(entry.body, rich)}</p>
      {entry.artifacts && entry.artifacts.length > 0 ? (
        <div className="flex flex-wrap gap-1 pt-0.5">
          {entry.artifacts.slice(0, 6).map((a) => (
            <Badge key={a} variant="outline" className="font-mono text-[9px]">
              {a}
            </Badge>
          ))}
          {entry.artifacts.length > 6 ? (
            <Badge variant="secondary" className="font-mono text-[9px]">
              +{entry.artifacts.length - 6}
            </Badge>
          ) : null}
        </div>
      ) : null}
      {synthesized ? (
        <span className="self-start text-[9px] uppercase tracking-wide text-muted-foreground/70">
          {entry.kind === 'comment' ? 'comment' : entry.kind}
        </span>
      ) : null}
    </article>
  );
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
