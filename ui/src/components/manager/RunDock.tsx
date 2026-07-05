import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type MouseEvent,
  type PointerEvent,
} from 'react';
import { ChevronDown, Maximize2, Minimize2, Power, X } from 'lucide-react';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { useActiveProject } from '@/hooks/useActiveProject';
import { useEventStream } from '@/hooks/useEventStream';
import { useMe } from '@/hooks/useMe';
import {
  fetchManagerDrivers,
  fetchManagerState,
  fetchRun,
  fetchRuns,
  isRunGoneError,
  postManagerLaunch,
  postRunRelease,
} from '@/lib/api';
import { useContainedWheelRef } from '@/lib/containedWheel';
import { MANAGER_TAB_ID, useRunDock } from '@/lib/runDock';
import { runDriverTag, runTabTitle } from '@/lib/runLabels';
import type { DaemonEvent, ManagerDriverProfile, ManagerSize, RunSummary } from '@/lib/types';
import { cn } from '@/lib/utils';
import { useResource } from '@/lib/useResource';

import { ManagerPeekBar } from './ManagerPeekBar';
import {
  ManagerWorkbench,
  managerLaunchArgs,
  managerLaunchModel,
  managerLaunchSystemWide,
} from './ManagerWorkbench';
import { RunningAgentsMenu } from './RunningAgentsMenu';
import { workerRunTabLabel } from './runDockLabels';
import { RunSurface } from './RunSurface';

const LAST_DRIVER_KEY = 'orgasmic.manager.driver';
const MANAGER_RESUME_DRAFT = '/orgasmic resume';
const MANAGER_DRIVER_PREFERENCE = [
  ['acp-stdio', 'hermes'],
  ['acp-stdio', 'codex'],
  ['tmux', 'claude'],
  ['tmux', 'codex'],
  ['tmux', 'cursor-agent'],
];

function driverStorageKey(driver: { mode: string; harness: string }): string {
  return `${driver.mode}\u0000${driver.harness}`;
}

function isManagerInputCapable(driver: ManagerDriverProfile): boolean {
  if (!driver.installed || driver.mode_installed === false) return false;
  // tmux and rmux both attach interactively through the daemon PTY bridge.
  if (driver.mode === 'tmux' || driver.mode === 'rmux') return true;
  return (
    driver.mode === 'acp-stdio' && (driver.harness === 'codex' || driver.harness === 'hermes')
  );
}

function resolveLaunchDriver(
  installed: ManagerDriverProfile[],
): ManagerDriverProfile | null {
  const candidates = installed.filter(isManagerInputCapable);
  if (candidates.length === 0) return null;
  const lastKey =
    typeof window !== 'undefined' ? window.localStorage.getItem(LAST_DRIVER_KEY) : null;
  const last = candidates.find((driver) => driverStorageKey(driver) === lastKey);
  if (last) return last;
  for (const [mode, harness] of MANAGER_DRIVER_PREFERENCE) {
    const preferred = candidates.find(
      (driver) => driver.mode === mode && driver.harness === harness,
    );
    if (preferred) return preferred;
  }
  return candidates[0] ?? null;
}

// Three visibly distinct heights. Peek has two sub-states: the resting manager
// bar (a one-line handle) and a windowed peek that hosts an opened run's live
// transcript as a small overlay above the dock (dec_053). Workbench is roughly
// half the viewport; focus is a full-viewport takeover.
function heightClass(size: ManagerSize, peekWindowed: boolean): string {
  if (size === 'focus') return 'h-screen';
  if (size === 'workbench') return 'h-[80vh] md:h-[min(56vh,540px)]';
  if (peekWindowed) return 'h-[42vh] md:h-[min(34vh,320px)]';
  return 'h-[70px] md:h-20';
}

function shouldToggleFromHeaderClick(event: MouseEvent<HTMLElement>): boolean {
  const target = event.target;
  if (!(target instanceof HTMLElement)) return false;
  return !target.closest('button,a,input,textarea,select,[role="button"],[role="tab"],[data-rundock-control]');
}

export function RunDock() {
  const { activeProjectId } = useActiveProject();
  const { can } = useMe();
  // The dock only renders when the viewer may watch sessions (see AppShell). A
  // member who can watch but lacks sessions.interact gets a read-only surface:
  // no composer, no PTY input, no launch/stop. Admin ⇒ can() true ⇒ interactive.
  const readOnly = !can(activeProjectId, 'sessions.interact');
  const {
    size,
    tabs,
    activeTabId,
    setSize,
    setActiveTab,
    openManager,
    openRun,
    closeTab,
    consumeDraft,
  } = useRunDock();
  const [tickBusy, setTickBusy] = useState(false);
  const [runLabelCache, setRunLabelCache] = useState<Record<string, string>>({});
  const dockRef = useContainedWheelRef<HTMLElement>();
  const dragStartRef = useRef<number | null>(null);
  const dragCurrentRef = useRef<number | null>(null);
  const dragCaptureRef = useRef<{ element: HTMLButtonElement; pointerId: number } | null>(null);

  const manager = useResource('rundock-manager-state', fetchManagerState);
  // Worker tabs need run summaries (driver/kind/task) to label and render.
  const runs = useResource('rundock-runs', fetchRuns);

  const managerRuns = useMemo(() => {
    // The supervisor snapshot lists every live run (manager + workers); only
    // interactive manager sessions (task_id `manager.launch:<project>`) may
    // pin the Manager tab, or the first worker dispatch would hijack it.
    const all = (manager.data?.runs ?? []).filter((run) =>
      run.task_id.startsWith('manager.launch:'),
    );
    if (!activeProjectId) return all;
    return all.filter((run) => run.project_id === activeProjectId);
  }, [activeProjectId, manager.data?.runs]);
  const activeManagerRun = managerRuns[0] ?? null;
  const managerDetail = useResource(
    `rundock-manager-run:${activeManagerRun?.run_id ?? 'none'}`,
    () => fetchRun(activeManagerRun?.run_id ?? ''),
    { enabled: Boolean(activeManagerRun) },
  );

  const runById = useMemo(() => {
    const map = new Map<string, RunSummary>();
    for (const run of runs.data?.live ?? []) map.set(run.run_id, run);
    return map;
  }, [runs.data?.live]);

  useEffect(() => {
    const liveRuns = runs.data?.live ?? [];
    if (liveRuns.length === 0) return;
    setRunLabelCache((current) => {
      let changed = false;
      const next = { ...current };
      for (const run of liveRuns) {
        const label = runTabTitle(run);
        if (next[run.run_id] !== label) {
          next[run.run_id] = label;
          changed = true;
        }
      }
      return changed ? next : current;
    });
  }, [runs.data?.live]);

  const refresh = useCallback(() => {
    void manager.refresh();
    void runs.refresh();
  }, [manager, runs]);

  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        if (event.topic === 'manager') {
          void manager.refresh();
          if (activeManagerRun) void managerDetail.refresh();
          return;
        }
        if (event.topic !== 'run') return;
        // A run crossing a lifecycle boundary (acquire/release/reattach) can
        // be the pinned manager run dying — e.g. a supervisor-side release —
        // so the manager snapshot must refresh too, not just the worker list.
        if (event.payload.kind === 'run_lifecycle') {
          void manager.refresh();
          void runs.refresh();
          return;
        }
        if (event.payload.kind !== 'run_event') {
          void runs.refresh();
        }
      },
      [activeManagerRun, manager, managerDetail, runs],
    ),
  );

  // Keyboard: Cmd/Ctrl+` toggles the dock; Escape collapses to peek.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const active = document.activeElement;
      const editing =
        active instanceof HTMLElement &&
        (active.isContentEditable ||
          active instanceof HTMLInputElement ||
          active instanceof HTMLTextAreaElement);
      if ((event.metaKey || event.ctrlKey) && event.code === 'Backquote' && !editing) {
        event.preventDefault();
        setSize(size === 'peek' ? 'workbench' : 'peek');
      }
      if (event.key === 'Escape' && size === 'focus' && !editing) {
        event.preventDefault();
        setSize('workbench');
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [setSize, size]);

  const finishDrag = useCallback((event?: PointerEvent<HTMLButtonElement>, cancelled = false) => {
    const start = dragStartRef.current;
    const current = dragCurrentRef.current ?? event?.clientY ?? null;
    const capture = dragCaptureRef.current;
    const element = event?.currentTarget ?? capture?.element;
    const pointerId = event?.pointerId ?? capture?.pointerId;
    if (element && pointerId !== undefined) {
      try {
        if (element.hasPointerCapture(pointerId)) element.releasePointerCapture(pointerId);
      } catch {
        /* Interrupted pointer captures may already be gone. */
      }
    }
    dragStartRef.current = null;
    dragCurrentRef.current = null;
    dragCaptureRef.current = null;
    if (cancelled || start === null || current === null) return null;
    return { start, current };
  }, []);

  useEffect(() => () => void finishDrag(undefined, true), [finishDrag]);

  const pointerHandlers = {
    onPointerDown: (event: PointerEvent<HTMLButtonElement>) => {
      dragStartRef.current = event.clientY;
      dragCurrentRef.current = event.clientY;
      dragCaptureRef.current = { element: event.currentTarget, pointerId: event.pointerId };
      event.currentTarget.setPointerCapture(event.pointerId);
    },
    onPointerMove: (event: PointerEvent<HTMLButtonElement>) => {
      if (dragStartRef.current === null) return;
      dragCurrentRef.current = event.clientY;
    },
    onPointerUp: (event: PointerEvent<HTMLButtonElement>) => {
      const drag = finishDrag(event);
      if (!drag) return;
      const delta = drag.start - drag.current;
      const viewportY = drag.current / window.innerHeight;
      if (delta > 100 || viewportY < 0.35) setSize(size === 'workbench' ? 'focus' : 'workbench');
      else if (delta < -60) setSize('peek');
    },
    onPointerCancel: (event: PointerEvent<HTMLButtonElement>) => void finishDrag(event, true),
  };

  async function handleOpenOrLaunchManager() {
    if (activeManagerRun) {
      openManager('workbench');
      return;
    }
    // A read-only member can watch an existing manager but never launch one.
    if (readOnly) return;
    if (!activeProjectId) {
      toast.error('Select a project before launching the manager');
      return;
    }
    setTickBusy(true);
    try {
      const drivers = await fetchManagerDrivers();
      const installed = drivers.drivers.filter((driver) => driver.installed);
      const driver = resolveLaunchDriver(installed);
      if (!driver) {
        toast.error('No input-capable manager driver installed');
        return;
      }
      const result = await postManagerLaunch({
        project_id: activeProjectId,
        mode: driver.mode,
        harness: driver.harness,
        model: managerLaunchModel(driver),
        harness_args: managerLaunchArgs(driver),
        system_wide: managerLaunchSystemWide(driver),
      });
      openRun({
        role: 'manager',
        runId: result.run_id,
        // A bare terminal session is not an orgasmic manager agent — no
        // resume-skill prompt to pre-fill.
        draftPrompt: driver.harness === 'custom' ? undefined : MANAGER_RESUME_DRAFT,
        size: 'workbench',
      });
      toast.success('Manager launched');
      await manager.refresh();
    } catch (err) {
      toast.error('Manager launch failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setTickBusy(false);
    }
  }

  async function handleStopManager() {
    if (!activeManagerRun) return;
    setTickBusy(true);
    try {
      await postRunRelease(activeManagerRun.run_id);
      toast.success('Manager stopped');
      await manager.refresh();
    } catch (err) {
      if (isRunGoneError(err)) {
        // The run already ended daemon-side; the pin was stale. Refreshing
        // clears it and brings the launcher back.
        toast.info('Manager run already ended');
      } else {
        toast.error('Stopping manager failed', {
          description: err instanceof Error ? err.message : String(err),
        });
      }
      await manager.refresh();
    } finally {
      setTickBusy(false);
    }
  }

  const activeTab = tabs.find((tab) => tab.tabId === activeTabId) ?? tabs[0];
  const activeWorkerRun =
    activeTab && activeTab.tabId !== MANAGER_TAB_ID && activeTab.runId
      ? runById.get(activeTab.runId) ?? null
      : null;

  const managerSource = managerDetail.data?.source;
  const peekDriver = activeManagerRun ? runDriverTag(activeManagerRun, managerSource) : 'manager';
  const acquiring = Boolean(!manager.data?.acquisition_paused && managerRuns.length > 0);
  const toggleFold = useCallback(() => {
    setSize(size === 'peek' ? 'workbench' : size === 'focus' ? 'workbench' : 'peek');
  }, [setSize, size]);

  // Peek is "windowed" when a worker run is the active tab: instead of the resting
  // one-line manager bar, we render that run's live transcript in a small overlay
  // (dec_053). The resting manager bar is reserved for when the Manager tab itself
  // is active in peek.
  const peekWindowed = size === 'peek' && Boolean(activeWorkerRun);

  return (
    <aside
      ref={dockRef}
      id="run-dock"
      tabIndex={-1}
      className={cn(
        'fixed inset-x-0 bottom-0 z-30 overscroll-contain border-t bg-background shadow-lg transition-[height] duration-200',
        // Edge-to-edge on Android: the safe-area padding below keeps controls
        // clear of the navigation bar while the dock background still fills down
        // to the screen edge. For the partial-height sizes box-content grows the
        // panel by the inset (content stays uncropped); full-screen focus mode
        // keeps border-box so it pads inward within the viewport. No-op off-Android.
        size === 'focus' ? 'box-border' : 'box-content',
        heightClass(size, peekWindowed),
      )}
      style={{
        paddingBottom: 'var(--safe-bottom)',
        paddingLeft: 'var(--safe-left)',
        paddingRight: 'var(--safe-right)',
        ...(size === 'focus' ? { paddingTop: 'var(--safe-top)' } : null),
      }}
      role={size === 'focus' ? 'dialog' : undefined}
      aria-modal={size === 'focus' ? true : undefined}
      aria-label="Run Dock"
    >
      {size === 'peek' && !peekWindowed ? (
        <ManagerPeekBar
          driverTag={peekDriver}
          runCount={managerRuns.length}
          acquiring={acquiring}
          tickBusy={tickBusy}
          onTick={() => void handleOpenOrLaunchManager()}
          onExpand={() => openManager('workbench')}
          expanded={false}
          controlsId="run-dock"
          {...pointerHandlers}
        />
      ) : peekWindowed && activeWorkerRun ? (
        <div className="flex h-full min-h-0 flex-col">
          <PeekWindowHeader
            label={runTabTitle(activeWorkerRun)}
            runId={activeWorkerRun.run_id}
            size={size}
            onSetSize={setSize}
            onToggleFold={toggleFold}
            onClose={() => activeTab && closeTab(activeTab.tabId)}
            onPointerDown={pointerHandlers.onPointerDown}
            onPointerMove={pointerHandlers.onPointerMove}
            onPointerUp={pointerHandlers.onPointerUp}
            onPointerCancel={pointerHandlers.onPointerCancel}
          />
          <div className="min-h-0 flex-1">
            <RunSurface
              run={activeWorkerRun}
              initialDraft={activeTab?.draftPrompt}
              onPromptSent={() => activeTab && consumeDraft(activeTab.tabId)}
              readOnly={readOnly}
            />
          </div>
        </div>
      ) : (
        <div className="flex h-full min-h-0 flex-col">
          <DockTabStrip
            tabs={tabs.map((tab) => ({
              tabId: tab.tabId,
              label:
                tab.tabId === MANAGER_TAB_ID
                  ? activeManagerRun
                    ? runTabTitle(activeManagerRun, managerSource)
                    : 'Manager'
                  : workerRunTabLabel(
                    tab.runId,
                    tab.runId ? runById.get(tab.runId) : null,
                    runLabelCache,
                  ),
              runId: tab.runId,
              closable: tab.tabId !== MANAGER_TAB_ID,
            }))}
            activeTabId={activeTabId}
            size={size}
            onSelect={setActiveTab}
            onClose={closeTab}
            onSetSize={setSize}
            onToggleFold={toggleFold}
            onPointerDown={pointerHandlers.onPointerDown}
            onPointerMove={pointerHandlers.onPointerMove}
            onPointerUp={pointerHandlers.onPointerUp}
            onPointerCancel={pointerHandlers.onPointerCancel}
            runningAgents={<RunningAgentsMenu projectId={activeProjectId} />}
            managerControls={
              !readOnly && activeTabId === MANAGER_TAB_ID && activeManagerRun ? (
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-sm"
                  aria-label="Stop manager"
                  title="Stop manager"
                  disabled={tickBusy}
                  onClick={() => void handleStopManager()}
                >
                  <Power />
                </Button>
              ) : null
            }
          />
          <div className="min-h-0 flex-1">
            {activeTab?.tabId === MANAGER_TAB_ID ? (
              <ManagerWorkbench
                size={size}
                projectId={activeProjectId}
                runs={managerRuns}
                activeRun={activeManagerRun}
                readOnly={readOnly}
                legacyDriverTag={
                  activeManagerRun ? runDriverTag(activeManagerRun, managerSource) : 'chat'
                }
                initialSource={managerDetail.data?.source}
                tickBusy={tickBusy}
                hideChrome
                initialDraft={activeTab.draftPrompt}
                onConsumeDraft={() => consumeDraft(MANAGER_TAB_ID)}
                onSelectRun={() => undefined}
                onSetSize={setSize}
                onTickStart={() => setTickBusy(true)}
                onTickEnd={async () => {
                  await manager.refresh();
                  setTickBusy(false);
                }}
              />
            ) : activeWorkerRun ? (
              <RunSurface
                run={activeWorkerRun}
                initialDraft={activeTab?.draftPrompt}
                onPromptSent={() => activeTab && consumeDraft(activeTab.tabId)}
                readOnly={readOnly}
              />
            ) : (
              <MissingRunPanel
                runId={activeTab?.runId ?? null}
                onRefresh={refresh}
                onClose={() => activeTab && closeTab(activeTab.tabId)}
              />
            )}
          </div>
        </div>
      )}
    </aside>
  );
}

type DockTabView = {
  tabId: string;
  label: string;
  runId: string | null;
  closable: boolean;
};

function DockTabStrip({
  tabs,
  activeTabId,
  size,
  onSelect,
  onClose,
  onSetSize,
  onToggleFold,
  onPointerDown,
  onPointerMove,
  onPointerUp,
  onPointerCancel,
  runningAgents,
  managerControls,
}: {
  tabs: DockTabView[];
  activeTabId: string;
  size: ManagerSize;
  onSelect: (tabId: string) => void;
  onClose: (tabId: string) => void;
  onSetSize: (size: ManagerSize) => void;
  onToggleFold: () => void;
  onPointerDown: (event: PointerEvent<HTMLButtonElement>) => void;
  onPointerMove: (event: PointerEvent<HTMLButtonElement>) => void;
  onPointerUp: (event: PointerEvent<HTMLButtonElement>) => void;
  onPointerCancel: (event: PointerEvent<HTMLButtonElement>) => void;
  runningAgents: React.ReactNode;
  managerControls: React.ReactNode;
}) {
  return (
    <header
      className="flex shrink-0 cursor-pointer items-center gap-2 border-b px-2 py-1.5"
      onClick={(event) => {
        if (shouldToggleFromHeaderClick(event)) onToggleFold();
      }}
    >
      <button
        type="button"
        className="flex h-9 w-7 shrink-0 touch-none items-center justify-center rounded-md text-muted-foreground hover:bg-muted md:h-7"
        aria-label="Resize run dock"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerCancel}
      >
        <span className="h-1 w-5 rounded-full bg-border" aria-hidden />
      </button>
      <div className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto" role="tablist">
        {tabs.map((tab) => {
          const active = tab.tabId === activeTabId;
          return (
            <div
              key={tab.tabId}
              data-rundock-control
              className={cn(
                'group flex shrink-0 items-center gap-1 rounded-md border px-2 py-1 text-xs transition-colors',
                active
                  ? 'border-primary/40 bg-accent text-accent-foreground'
                  : 'border-transparent text-muted-foreground hover:bg-muted',
              )}
            >
              <Tooltip>
                <TooltipTrigger asChild>
                  <button
                    type="button"
                    role="tab"
                    aria-selected={active}
                    className="max-w-[180px] truncate font-medium outline-none"
                    onClick={() => onSelect(tab.tabId)}
                  >
                    {tab.label}
                  </button>
                </TooltipTrigger>
                {tab.runId ? (
                  <TooltipContent side="top" className="font-mono text-[10px]">
                    {tab.runId}
                  </TooltipContent>
                ) : null}
              </Tooltip>
              {tab.closable ? (
                <button
                  type="button"
                  className="rounded-sm p-0.5 text-muted-foreground/70 opacity-60 hover:bg-background hover:text-foreground group-hover:opacity-100"
                  aria-label={`Close tab ${tab.label}`}
                  title="Detach tab (does not stop the run)"
                  onClick={(event) => {
                    event.stopPropagation();
                    onClose(tab.tabId);
                  }}
                >
                  <X className="size-3" />
                </button>
              ) : null}
            </div>
          );
        })}
      </div>
      <div className="ml-auto flex shrink-0 items-center gap-1" data-rundock-control>
        {runningAgents}
        {managerControls}
        <span className="mx-1 h-5 w-px bg-border" aria-hidden />
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Collapse dock"
          title="Collapse dock"
          onClick={(event) => {
            event.stopPropagation();
            onToggleFold();
          }}
        >
          <ChevronDown />
        </Button>
        <FullscreenControl size={size} onSetSize={onSetSize} />
      </div>
    </header>
  );
}

function FullscreenControl({
  size,
  onSetSize,
}: {
  size: ManagerSize;
  onSetSize: (size: ManagerSize) => void;
}) {
  const focused = size === 'focus';
  return (
    <Button
      type="button"
      variant={focused ? 'secondary' : 'ghost'}
      size="icon-sm"
      aria-label={focused ? 'Exit full screen' : 'Full screen'}
      aria-pressed={focused}
      title={focused ? 'Exit full screen' : 'Full screen'}
      onClick={(event) => {
        event.stopPropagation();
        onSetSize(focused ? 'workbench' : 'focus');
      }}
    >
      {focused ? <Minimize2 /> : <Maximize2 />}
    </Button>
  );
}

// Header for the windowed peek state: a compact run-titled bar that hosts the
// drag handle, the run label, one fullscreen control, and a detach button.
// Peek-with-a-worker-run shows a real transcript window instead of silently
// falling back to the manager bar.
function PeekWindowHeader({
  label,
  runId,
  size,
  onSetSize,
  onToggleFold,
  onClose,
  onPointerDown,
  onPointerMove,
  onPointerUp,
  onPointerCancel,
}: {
  label: string;
  runId: string;
  size: ManagerSize;
  onSetSize: (size: ManagerSize) => void;
  onToggleFold: () => void;
  onClose: () => void;
  onPointerDown: (event: PointerEvent<HTMLButtonElement>) => void;
  onPointerMove: (event: PointerEvent<HTMLButtonElement>) => void;
  onPointerUp: (event: PointerEvent<HTMLButtonElement>) => void;
  onPointerCancel: (event: PointerEvent<HTMLButtonElement>) => void;
}) {
  return (
    <header
      className="flex shrink-0 cursor-pointer items-center gap-2 border-b px-2 py-1.5"
      onClick={(event) => {
        if (shouldToggleFromHeaderClick(event)) onToggleFold();
      }}
    >
      <button
        type="button"
        className="flex h-9 w-7 shrink-0 touch-none items-center justify-center rounded-md text-muted-foreground hover:bg-muted md:h-7"
        aria-label="Resize run dock"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerCancel}
      >
        <span className="h-1 w-5 rounded-full bg-border" aria-hidden />
      </button>
      <Tooltip>
        <TooltipTrigger asChild>
          <span className="min-w-0 flex-1 truncate text-xs font-medium" title={runId}>
            {label}
          </span>
        </TooltipTrigger>
        <TooltipContent side="top" className="font-mono text-[10px]">
          {runId}
        </TooltipContent>
      </Tooltip>
      <div className="ml-auto flex shrink-0 items-center gap-1" data-rundock-control>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Minimize to tab"
          title="Minimize to tab"
          onClick={(event) => {
            event.stopPropagation();
            onToggleFold();
          }}
        >
          <Minimize2 />
        </Button>
        <FullscreenControl size={size} onSetSize={onSetSize} />
        <span className="mx-1 h-5 w-px bg-border" aria-hidden />
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Detach tab"
          title="Detach tab (does not stop the run)"
          onClick={onClose}
        >
          <X />
        </Button>
      </div>
    </header>
  );
}

function MissingRunPanel({
  runId,
  onRefresh,
  onClose,
}: {
  runId: string | null;
  onRefresh: () => void;
  onClose: () => void;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 p-6 text-center">
      <p className="text-sm text-muted-foreground">
        Run {runId ? <code className="font-mono">{runId}</code> : 'this run'} is no longer live.
      </p>
      <div className="flex items-center gap-2">
        <Button type="button" variant="outline" size="sm" onClick={onRefresh}>
          Refresh
        </Button>
        <Button type="button" variant="ghost" size="sm" onClick={onClose}>
          Close tab
        </Button>
      </div>
    </div>
  );
}
