import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type PointerEvent,
  type ReactElement,
} from 'react';
import {
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  GitBranch,
  KanbanSquare,
  List,
  User,
} from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useIsMobile } from '@/hooks/use-mobile';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { useTaskRuns, type TaskRunMatch } from '@/hooks/useTaskRuns';
import { fetchProject } from '@/lib/api';
import { useQueryState } from '@/lib/routing';
import { getString, setString } from '@/lib/storage';
import type { LifecycleStage, TaskSummary, TasksLayout } from '@/lib/types';
import {
  LIFECYCLE_STAGES,
  lifecycleStageLabel,
} from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import { ErrorPanel } from './Primitives';
import { NestedTreeRow } from './NestedTreeRow';
import { TaskAgentBadges } from './TaskAgentBadges';
import { KANBAN_COLUMNS, kanbanStage } from './kanbanUtils';
import {
  buildTaskTree,
  countTaskTreeNodes,
  type TaskTreeNode,
} from './taskTree';

type BadgeVariant = 'default' | 'secondary' | 'destructive' | 'outline';
type ForTask = (taskId: string) => TaskRunMatch;

const LONG_PRESS_MS = 500;
const LONG_PRESS_CANCEL_PX = 8;
const TASK_LIST_PAGE_SIZE = 10;

function taskStageCollapsedKey(projectId: string, stage: string): string {
  return `tasks:list-stage-collapsed:${projectId}:${stage}`;
}

function readStoredStageCollapsed(projectId: string, stage: string): boolean {
  const stored = getString(taskStageCollapsedKey(projectId, stage));
  return stored === null ? true : stored === 'true';
}

function taskTreeCollapsedKey(projectId: string, taskId: string): string {
  return `tasks:list-tree-collapsed:${projectId}:${taskId}`;
}

function readStoredTaskCollapsed(projectId: string, taskId: string): boolean {
  return getString(taskTreeCollapsedKey(projectId, taskId)) === 'true';
}

export function stageVariant(stage: LifecycleStage | string): BadgeVariant {
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

function blockedByItems(task: TaskSummary): string[] {
  const raw = task.blocked_by;
  if (Array.isArray(raw)) return raw.map((item) => item.trim()).filter(Boolean);
  if (typeof raw === 'string') return raw.trim() ? [raw.trim()] : [];
  return [];
}

export function TasksPage({
  projectId,
  onSelectTask,
}: {
  projectId: string | null;
  onSelectTask: (taskId: string) => void;
}) {
  if (!projectId) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="text-base">No project selected</CardTitle>
        </CardHeader>
        <CardContent className="text-sm text-muted-foreground">
          Open a project from the Board to see its tasks.
        </CardContent>
      </Card>
    );
  }
  return <TasksPageBody projectId={projectId} onSelectTask={onSelectTask} />;
}

function TasksPageBody({
  projectId,
  onSelectTask,
}: {
  projectId: string;
  onSelectTask: (taskId: string) => void;
}) {
  const [layout, setLayout] = useQueryState('layout', 'list');
  const isMobile = useIsMobile();
  const refresh = useRefreshToken();
  const taskRuns = useTaskRuns();
  const { data, error, loading } = useResource(
    `tasks:${projectId}:${refresh}`,
    () => fetchProject(projectId),
  );

  if (loading && !data) {
    return (
      <div className="flex flex-col gap-3">
        <Skeleton className="h-10" />
        <Skeleton className="h-96" />
      </div>
    );
  }
  if (error) return <ErrorPanel error={error} />;
  if (!data) return null;

  const tasks = data.tasks ?? [];
  const requestedLayout: TasksLayout = layout === 'kanban' ? 'kanban' : 'list';

  return (
    <div className="flex flex-col gap-4">
      <Toolbar
        layout={requestedLayout}
        setLayout={setLayout}
        taskCount={tasks.length}
        doneCount={tasks.filter((t) => t.lifecycle_stage === 'done').length}
        activeTotal={tasks.filter((t) => t.lifecycle_stage !== 'cancelled').length}
      />
      {requestedLayout === 'kanban' ? (
        <KanbanBoard
          tasks={tasks}
          onSelectTask={onSelectTask}
          longPressEnabled={isMobile}
          forTask={taskRuns.forTask}
        />
      ) : (
        <TaskList
          projectId={projectId}
          tasks={tasks}
          onSelectTask={onSelectTask}
          longPressEnabled={isMobile}
          forTask={taskRuns.forTask}
        />
      )}
    </div>
  );
}

function useTaskLongPress(
  taskId: string,
  enabled: boolean,
  onSelectTask: (id: string) => void,
) {
  const timerRef = useRef<number | null>(null);
  const startRef = useRef<{ x: number; y: number } | null>(null);
  const firedRef = useRef(false);

  const clear = useCallback(() => {
    if (timerRef.current !== null) window.clearTimeout(timerRef.current);
    timerRef.current = null;
    startRef.current = null;
  }, []);

  useEffect(() => clear, [clear]);

  const onPointerDown = useCallback(
    (event: PointerEvent<HTMLElement>) => {
      if (!enabled || event.pointerType === 'mouse') return;
      clear();
      firedRef.current = false;
      startRef.current = { x: event.clientX, y: event.clientY };
      timerRef.current = window.setTimeout(() => {
        firedRef.current = true;
        onSelectTask(taskId);
      }, LONG_PRESS_MS);
    },
    [clear, enabled, onSelectTask, taskId],
  );

  const onPointerMove = useCallback(
    (event: PointerEvent<HTMLElement>) => {
      const start = startRef.current;
      if (!start) return;
      if (
        Math.abs(event.clientX - start.x) > LONG_PRESS_CANCEL_PX ||
        Math.abs(event.clientY - start.y) > LONG_PRESS_CANCEL_PX
      ) {
        clear();
      }
    },
    [clear],
  );

  const onClickCapture = useCallback((event: MouseEvent<HTMLElement>) => {
    if (!firedRef.current) return;
    event.preventDefault();
    event.stopPropagation();
    firedRef.current = false;
  }, []);

  return {
    onPointerDown,
    onPointerMove,
    onPointerUp: clear,
    onPointerCancel: clear,
    onPointerLeave: clear,
    onClickCapture,
  };
}

function Toolbar({
  layout,
  setLayout,
  taskCount,
  doneCount,
  activeTotal,
}: {
  layout: TasksLayout;
  setLayout: (v: string) => void;
  taskCount: number;
  /** Tasks in the `done` stage. */
  doneCount: number;
  /** All tasks except `cancelled` — the denominator for done-progress. */
  activeTotal: number;
}) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex min-w-0 items-baseline gap-2">
        <h2 className="text-lg font-semibold tracking-tight">Tasks</h2>
        <Badge variant="outline" className="font-mono text-[10px]">
          {taskCount} task{taskCount === 1 ? '' : 's'}
        </Badge>
        {activeTotal > 0 && doneCount > 0 ? (
          <span className="flex items-center gap-1.5 self-center text-xs text-muted-foreground">
            <span
              className="hidden h-1.5 w-16 overflow-hidden rounded-full bg-muted sm:block"
              role="progressbar"
              aria-label="Tasks done"
              aria-valuemin={0}
              aria-valuemax={activeTotal}
              aria-valuenow={doneCount}
            >
              <span
                className="block h-full rounded-full bg-primary"
                style={{ width: `${Math.round((doneCount / activeTotal) * 100)}%` }}
              />
            </span>
            <span className="font-mono">{doneCount}/{activeTotal} done</span>
          </span>
        ) : null}
      </div>
      <div className="flex items-center gap-1 rounded-md border bg-muted/30 p-0.5">
        <Button
          variant={layout === 'list' ? 'secondary' : 'ghost'}
          size="sm"
          onClick={() => setLayout('list')}
          className="h-7 gap-1.5 px-2"
        >
          <List className="size-3.5" />
          List
        </Button>
        <Button
          variant={layout === 'kanban' ? 'secondary' : 'ghost'}
          size="sm"
          onClick={() => setLayout('kanban')}
          className="h-7 gap-1.5 px-2"
        >
          <KanbanSquare className="size-3.5" />
          Kanban
        </Button>
      </div>
    </div>
  );
}

function TaskList({
  projectId,
  tasks,
  onSelectTask,
  longPressEnabled,
  forTask,
}: {
  projectId: string;
  tasks: TaskSummary[];
  onSelectTask: (id: string) => void;
  longPressEnabled: boolean;
  forTask: ForTask;
}) {
  const groups = useMemo(() => {
    const m = new Map<string, TaskTreeNode[]>();
    for (const node of buildTaskTree(tasks)) {
      const k = node.task.lifecycle_stage ?? 'backlog';
      (m.get(k) ?? m.set(k, []).get(k))!.push(node);
    }
    const order: string[] = LIFECYCLE_STAGES.filter((s) => m.has(s));
    for (const k of [...m.keys()].sort()) if (!order.includes(k)) order.push(k);
    return order.map((s) => [s, m.get(s)!] as const);
  }, [tasks]);

  if (tasks.length === 0) {
    return (
      <Card>
        <CardContent className="px-6 py-10 text-center text-sm text-muted-foreground">
          No tasks yet. File the first with{' '}
          <code className="font-mono text-foreground">orgasmic task create</code>, or set a goal with{' '}
          <code className="font-mono text-foreground">orgasmic goal set</code> and let the manager
          plan the backlog.
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {groups.map(([stage, items]) => (
        <Card key={stage} className="overflow-hidden bg-transparent p-0">
          <TaskStageSection
            projectId={projectId}
            stage={stage}
            items={items}
            onSelectTask={onSelectTask}
            longPressEnabled={longPressEnabled}
            forTask={forTask}
          />
        </Card>
      ))}
    </div>
  );
}

function TaskStageSection({
  projectId,
  stage,
  items,
  onSelectTask,
  longPressEnabled,
  forTask,
}: {
  projectId: string;
  stage: string;
  items: TaskTreeNode[];
  onSelectTask: (id: string) => void;
  longPressEnabled: boolean;
  forTask: ForTask;
}) {
  const storageKey = taskStageCollapsedKey(projectId, stage);
  const [collapseState, setCollapseState] = useState(() => ({
    collapsed: readStoredStageCollapsed(projectId, stage),
    storageKey,
  }));
  const collapsed =
    collapseState.storageKey === storageKey
      ? collapseState.collapsed
      : readStoredStageCollapsed(projectId, stage);
  const [page, setPage] = useState(0);
  const taskCount = countTaskTreeNodes(items);
  const pageCount = Math.max(1, Math.ceil(items.length / TASK_LIST_PAGE_SIZE));
  const safePage = Math.min(page, pageCount - 1);
  const firstItem = safePage * TASK_LIST_PAGE_SIZE;
  const visibleItems = items.slice(firstItem, firstItem + TASK_LIST_PAGE_SIZE);
  const rangeStart = items.length === 0 ? 0 : firstItem + 1;
  const rangeEnd = Math.min(items.length, firstItem + TASK_LIST_PAGE_SIZE);

  useEffect(() => {
    setPage((current) => Math.min(current, pageCount - 1));
  }, [pageCount]);

  useEffect(() => {
    if (collapseState.storageKey === storageKey) return;
    setCollapseState({ collapsed: readStoredStageCollapsed(projectId, stage), storageKey });
  }, [collapseState.storageKey, projectId, stage, storageKey]);

  useEffect(() => {
    if (collapseState.storageKey !== storageKey) return;
    setString(storageKey, String(collapseState.collapsed));
  }, [collapseState, storageKey]);

  return (
    <section>
      <div className="flex min-h-11 flex-wrap items-center gap-2 bg-muted/30 px-3 py-2">
        <button
          type="button"
          onClick={() =>
            setCollapseState((current) => ({
              collapsed:
                current.storageKey === storageKey
                  ? !current.collapsed
                  : !readStoredStageCollapsed(projectId, stage),
              storageKey,
            }))
          }
          aria-expanded={!collapsed}
          className="flex min-w-0 flex-1 items-center gap-2 rounded-sm px-1 py-1 text-left transition-colors hover:bg-muted/60 focus-visible:bg-muted/60 focus-visible:outline-none"
        >
          {collapsed ? (
            <ChevronRight className="size-4 shrink-0 text-muted-foreground" />
          ) : (
            <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
          )}
          <Badge variant={stageVariant(stage)} className="capitalize">
            {lifecycleStageLabel(stage)}
          </Badge>
          <span className="text-xs text-muted-foreground">{taskCount}</span>
          {taskCount !== items.length ? (
            <span className="hidden text-[10px] text-muted-foreground sm:inline">
              · {items.length} top-level
            </span>
          ) : null}
        </button>
        {!collapsed && items.length > TASK_LIST_PAGE_SIZE ? (
          <div className="ml-auto flex items-center gap-1">
            <span className="hidden font-mono text-[10px] text-muted-foreground sm:inline">
              {rangeStart}-{rangeEnd} / {items.length} top-level
            </span>
            <Button
              variant="ghost"
              size="icon"
              className="size-7"
              disabled={safePage === 0}
              onClick={() => setPage((value) => Math.max(0, value - 1))}
              aria-label={`Previous ${lifecycleStageLabel(stage)} top-level tasks`}
            >
              <ChevronLeft className="size-3.5" />
            </Button>
            <span className="min-w-10 text-center font-mono text-[10px] text-muted-foreground">
              {safePage + 1}/{pageCount}
            </span>
            <Button
              variant="ghost"
              size="icon"
              className="size-7"
              disabled={safePage >= pageCount - 1}
              onClick={() => setPage((value) => Math.min(pageCount - 1, value + 1))}
              aria-label={`Next ${lifecycleStageLabel(stage)} top-level tasks`}
            >
              <ChevronRight className="size-3.5" />
            </Button>
          </div>
        ) : null}
      </div>
      {collapsed ? null : (
        <ul>
          {visibleItems.map((node) => (
            <TaskTreeItem
              key={node.task.id}
              projectId={projectId}
              sectionStage={stage}
              node={node}
              depth={0}
              onSelectTask={onSelectTask}
              longPressEnabled={longPressEnabled}
              forTask={forTask}
            />
          ))}
        </ul>
      )}
    </section>
  );
}

function TaskTreeItem({
  projectId,
  sectionStage,
  node,
  depth,
  onSelectTask,
  longPressEnabled,
  forTask,
}: {
  projectId: string;
  sectionStage: string;
  node: TaskTreeNode;
  depth: number;
  onSelectTask: (id: string) => void;
  longPressEnabled: boolean;
  forTask: ForTask;
}) {
  const { task, children } = node;
  const match = forTask(task.id);
  const hasChildren = children.length > 0;
  const storageKey = taskTreeCollapsedKey(projectId, task.id);
  const [collapseState, setCollapseState] = useState(() => ({
    collapsed: readStoredTaskCollapsed(projectId, task.id),
    storageKey,
  }));
  const collapsed =
    collapseState.storageKey === storageKey
      ? collapseState.collapsed
      : readStoredTaskCollapsed(projectId, task.id);
  const longPress = useTaskLongPress(task.id, longPressEnabled, onSelectTask);
  const childrenId = `task-children-${projectId}-${task.id}`;
  const childCount = children.length;

  useEffect(() => {
    if (collapseState.storageKey === storageKey) return;
    setCollapseState({
      collapsed: readStoredTaskCollapsed(projectId, task.id),
      storageKey,
    });
  }, [collapseState.storageKey, projectId, storageKey, task.id]);

  useEffect(() => {
    if (collapseState.storageKey !== storageKey) return;
    setString(storageKey, String(collapseState.collapsed));
  }, [collapseState, storageKey]);

  return (
    <li>
      <NestedTreeRow
        depth={depth}
        nodeId={task.id}
        nodeKind="task"
        title={task.title}
        hasChildren={hasChildren}
        expanded={!collapsed}
        childrenId={childrenId}
        toggleLabel={`${collapsed ? 'Expand' : 'Collapse'} ${task.title}`}
        onToggle={() =>
          setCollapseState((current) => ({
            collapsed:
              current.storageKey === storageKey
                ? !current.collapsed
                : !readStoredTaskCollapsed(projectId, task.id),
            storageKey,
          }))
        }
        onOpen={() => onSelectTask(task.id)}
        openLabel={`Open ${task.id}: ${task.title}`}
        openProps={longPress}
        meta={
          <TaskMetaChips
            task={task}
            hideOnSmall
            hasLiveRun={match.running.length > 0}
            showParent={false}
            sectionStage={sectionStage}
            childCount={childCount}
          />
        }
        corner={<TaskAgentBadges match={match} />}
      />
      {hasChildren && !collapsed ? (
        <ul id={childrenId}>
          {children.map((child) => (
            <TaskTreeItem
              key={child.task.id}
              projectId={projectId}
              sectionStage={sectionStage}
              node={child}
              depth={depth + 1}
              onSelectTask={onSelectTask}
              longPressEnabled={longPressEnabled}
              forTask={forTask}
            />
          ))}
        </ul>
      ) : null}
    </li>
  );
}

function KanbanBoard({
  tasks,
  onSelectTask,
  longPressEnabled,
  forTask,
}: {
  tasks: TaskSummary[];
  onSelectTask: (id: string) => void;
  longPressEnabled: boolean;
  forTask: ForTask;
}) {
  const byStage = useMemo(() => {
    const m = new Map<string, TaskSummary[]>();
    for (const stage of KANBAN_COLUMNS) m.set(stage, []);
    for (const t of tasks) {
      const stage = kanbanStage(t.lifecycle_stage);
      if (stage) m.get(stage)!.push(t);
    }
    return m;
  }, [tasks]);

  return (
    <div className="h-[calc(100vh-10rem)] w-full overflow-x-auto overscroll-x-contain pb-2">
      <div className="flex h-full min-h-[24rem] w-max min-w-full gap-3 pb-3">
        {KANBAN_COLUMNS.map((stage) => {
          const items = byStage.get(stage) ?? [];
          return (
            <KanbanColumn
              key={stage}
              stage={stage}
              items={items}
              onSelectTask={onSelectTask}
              longPressEnabled={longPressEnabled}
              forTask={forTask}
            />
          );
        })}
      </div>
    </div>
  );
}

function KanbanColumn({
  stage,
  items,
  onSelectTask,
  longPressEnabled,
  forTask,
}: {
  stage: LifecycleStage;
  items: TaskSummary[];
  onSelectTask: (id: string) => void;
  longPressEnabled: boolean;
  forTask: ForTask;
}) {
  return (
    <div className="flex w-72 shrink-0 flex-col rounded-md border bg-muted/20">
      <div className="flex items-center gap-2 border-b bg-background/60 px-3 py-2">
        <Badge variant={stageVariant(stage)} className="capitalize">
          {lifecycleStageLabel(stage)}
        </Badge>
        <span className="text-xs text-muted-foreground">{items.length}</span>
      </div>
      <div className="flex flex-1 flex-col gap-2 overflow-y-auto p-2">
        {items.length === 0 ? (
          <div className="rounded border border-dashed py-6 text-center text-xs text-muted-foreground">
            empty
          </div>
        ) : (
          items.map((t) => (
            <KanbanCard
              key={t.id}
              task={t}
              onClick={() => onSelectTask(t.id)}
              onSelectTask={onSelectTask}
              longPressEnabled={longPressEnabled}
              match={forTask(t.id)}
            />
          ))
        )}
      </div>
    </div>
  );
}

function KanbanCard({
  task,
  onClick,
  onSelectTask,
  longPressEnabled,
  match,
}: {
  task: TaskSummary;
  onClick: () => void;
  onSelectTask: (id: string) => void;
  longPressEnabled: boolean;
  match: TaskRunMatch;
}) {
  const longPress = useTaskLongPress(task.id, longPressEnabled, onSelectTask);
  const hasAgents = match.running.length > 0;
  const blockedBy = blockedByItems(task);

  return (
    <div className="relative">
      <button
        type="button"
        {...longPress}
        onClick={onClick}
        className="flex w-full flex-col gap-1.5 rounded-md border bg-background p-2.5 text-left shadow-sm transition-all hover:border-primary/60 hover:shadow focus-visible:border-primary/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
      >
        <div className="flex items-center justify-between gap-2">
          <code className="font-mono text-[10px] text-muted-foreground">{task.id}</code>
          {task.priority ? (
            <Badge variant="secondary" className="font-mono text-[10px]">
              {task.priority}
            </Badge>
          ) : null}
        </div>
        <p className="whitespace-normal break-words text-sm leading-snug">{task.title}</p>
        {blockedBy.length > 0 ? (
          <Badge
            variant="destructive"
            className="w-fit font-mono text-[10px]"
            title={`Blocked by ${blockedBy.join(', ')}`}
          >
            Blocked
          </Badge>
        ) : null}
        <TaskMetaChips task={task} hasLiveRun={hasAgents} />
        {hasAgents ? <span className="h-5" aria-hidden /> : null}
      </button>
      {hasAgents ? (
        <div className="absolute bottom-2 right-2.5">
          <TaskAgentBadges match={match} />
        </div>
      ) : null}
    </div>
  );
}

// orgasmic:task_W43NY,dec_QWEQ8
function TaskMetaChips({
  task,
  hideOnSmall = false,
  hasLiveRun = false,
  showParent = true,
  sectionStage,
  childCount = 0,
}: {
  task: TaskSummary;
  hideOnSmall?: boolean;
  // When a live run exists, owner collapses into the performer pill rendered
  // by TaskAgentBadges. Hierarchy callers can also suppress the parent chip.
  hasLiveRun?: boolean;
  showParent?: boolean;
  sectionStage?: string;
  childCount?: number;
}) {
  const items: ReactElement[] = [];
  const hasDifferentStage = Boolean(
    sectionStage && task.lifecycle_stage !== sectionStage,
  );
  if (hasDifferentStage) {
    items.push(
      <Badge key="stage" variant={stageVariant(task.lifecycle_stage)} className="capitalize">
        {lifecycleStageLabel(task.lifecycle_stage)}
      </Badge>,
    );
  }
  if (childCount > 0) {
    items.push(
      <Badge
        key="children"
        variant="outline"
        className="gap-1 font-mono text-[10px]"
        title={`${childCount} subtask${childCount === 1 ? '' : 's'}`}
      >
        <GitBranch className="size-2.5" />
        {childCount}
      </Badge>,
    );
  }
  if (showParent && task.parent_task) {
    items.push(
      <Badge key="parent" variant="outline" className="gap-1 font-mono text-[10px]">
        <GitBranch className="size-2.5" />
        {task.parent_task}
      </Badge>,
    );
  }
  if (!hasLiveRun && task.owner && task.owner !== 'human') {
    items.push(
      <Badge key="owner" variant="default" className="gap-1 font-mono text-[10px]">
        <User className="size-2.5" />
        {task.owner}
      </Badge>,
    );
  }
  if (items.length === 0) return null;
  return (
    <div
      className={cn(
        'flex flex-wrap items-center gap-1',
        hideOnSmall && !hasDifferentStage && 'hidden shrink-0 sm:inline-flex',
        hideOnSmall && hasDifferentStage && 'shrink-0',
      )}
    >
      {items}
    </div>
  );
}
