import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent,
} from 'react';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import { useActiveProject } from '@/hooks/useActiveProject';
import { useEventStream } from '@/hooks/useEventStream';
import { useMe } from '@/hooks/useMe';
import {
  fetchManagerDrivers,
  fetchManagerState,
  fetchRuns,
  isRunGoneError,
  postManagerLaunch,
  postRunRelease,
} from '@/lib/api';
import { useContainedWheelRef } from '@/lib/containedWheel';
import { useRunDock } from '@/lib/runDock';
import { dockHeightFromPointer } from '@/lib/runDockUtils';
import { isManagerRun, runTabTitle } from '@/lib/runLabels';
import type { DaemonEvent, RunSummary } from '@/lib/types';
import { cn } from '@/lib/utils';
import { useResource } from '@/lib/useResource';

import { DockTaskbar, type TaskbarRunButton } from './DockTaskbar';
import { RunningAgentsMenu } from './RunningAgentsMenu';
import {
  isTerminalRun,
  orderRunsByLaunch,
  terminalRunLabel,
  workerButtonLabel,
  workerRunTabLabel,
} from './runDockLabels';
import { RunSurface } from './RunSurface';
import { launchSystemWide, resolveTerminalDriver } from './terminalLaunch';

export function RunDock() {
  const { activeProjectId } = useActiveProject();
  const { can } = useMe();
  // The dock only renders when the viewer may watch sessions (see AppShell). A
  // member who can watch but lacks sessions.interact gets a read-only surface:
  // no composer, no PTY input, no launch/stop. Admin ⇒ can() true ⇒ interactive.
  const readOnly = !can(activeProjectId, 'sessions.interact');
  const {
    open,
    height,
    tabs,
    activeTabId,
    setHeight,
    setActiveTab,
    openRun,
    minimize,
    closeTab,
    consumeDraft,
  } = useRunDock();
  const [terminalBusy, setTerminalBusy] = useState(false);
  // Height animates on raise/minimize but must track the pointer exactly while
  // dragging, so the transition is dropped for the duration of a resize.
  const [resizing, setResizing] = useState(false);
  const [runLabelCache, setRunLabelCache] = useState<Record<string, string>>({});
  const dockRef = useContainedWheelRef<HTMLElement>();
  const resizeCaptureRef = useRef<{ element: HTMLElement; pointerId: number } | null>(null);

  const manager = useResource('rundock-manager-state', fetchManagerState);
  // Run buttons need summaries (driver/kind/task) to label and render.
  const runs = useResource('rundock-runs', fetchRuns);

  const liveRuns = useMemo(() => runs.data?.live ?? [], [runs.data?.live]);
  const runById = useMemo(() => {
    const map = new Map<string, RunSummary>();
    for (const run of liveRuns) map.set(run.run_id, run);
    // The manager snapshot can be ahead of the runs list right after a launch
    // (its refresh resolves first); folding it in keeps a just-launched
    // terminal renderable instead of flashing the missing-run panel.
    for (const run of manager.data?.runs ?? []) {
      if (!map.has(run.run_id)) map.set(run.run_id, run);
    }
    return map;
  }, [liveRuns, manager.data?.runs]);

  // Every live run of the active project earns a taskbar button the moment it
  // is dispatched — the Windows-taskbar model. Only clicking a button opens a
  // surface, so dispatch stays quiet (dec_FBBT2's noise concern holds: buttons
  // appear, sessions never raise themselves).
  const projectRuns = useMemo(() => {
    const inProject = (run: RunSummary) =>
      !activeProjectId || !run.project_id || run.project_id === activeProjectId;
    const all = orderRunsByLaunch([...runById.values()].filter(inProject));
    // Managers and terminals share the manager.launch namespace; the `custom`
    // pseudo-harness is what separates a bare terminal from an agent manager.
    return {
      managers: all.filter((run) => isManagerRun(run) && !isTerminalRun(run)),
      terminals: all.filter(isTerminalRun),
      workers: all.filter((run) => !isManagerRun(run)),
    };
  }, [activeProjectId, runById]);

  useEffect(() => {
    if (liveRuns.length === 0) return;
    setRunLabelCache((current) => {
      let changed = false;
      const next = { ...current };
      for (const run of liveRuns) {
        const label = isTerminalRun(run) ? 'Terminal' : runTabTitle(run);
        if (next[run.run_id] !== label) {
          next[run.run_id] = label;
          changed = true;
        }
      }
      return changed ? next : current;
    });
  }, [liveRuns]);

  const refresh = useCallback(() => {
    void manager.refresh();
    void runs.refresh();
  }, [manager, runs]);

  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        if (event.topic === 'manager') {
          void manager.refresh();
          return;
        }
        if (event.topic !== 'run') return;
        // A run crossing a lifecycle boundary (acquire/release/reattach) can be
        // a manager dying — e.g. a supervisor-side release — so the manager
        // snapshot must refresh too, not just the worker list.
        if (event.payload.kind === 'run_lifecycle') {
          void manager.refresh();
          void runs.refresh();
          return;
        }
        if (event.payload.kind !== 'run_event') {
          void runs.refresh();
        }
      },
      [manager, runs],
    ),
  );

  const activeTab = tabs.find((tab) => tab.tabId === activeTabId) ?? null;
  const activeRun = activeTab ? runById.get(activeTab.runId) ?? null : null;

  const raiseLastTab = useCallback(() => {
    const target = activeTab ?? tabs[tabs.length - 1];
    if (target) openRun({ runId: target.runId });
  }, [activeTab, openRun, tabs]);

  // Keyboard: Cmd/Ctrl+` toggles the dock; Escape minimizes an open dock.
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
        if (open) minimize();
        else raiseLastTab();
      }
      // Escape belongs to the focused terminal/composer first; only an idle
      // Escape puts the dock away.
      if (event.key === 'Escape' && open && !editing) {
        event.preventDefault();
        minimize();
      }
    };
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [minimize, open, raiseLastTab]);

  const releaseResizeCapture = useCallback(() => {
    const capture = resizeCaptureRef.current;
    resizeCaptureRef.current = null;
    setResizing(false);
    if (!capture) return;
    try {
      if (capture.element.hasPointerCapture(capture.pointerId))
        capture.element.releasePointerCapture(capture.pointerId);
    } catch {
      /* Interrupted pointer captures may already be gone. */
    }
  }, []);

  useEffect(() => releaseResizeCapture, [releaseResizeCapture]);

  // Drag anywhere along the dock's top border to resize; dragging past the
  // bottom threshold puts the dock away entirely. All four handlers live on the
  // taskbar itself — pointer capture retargets move/up to the element that
  // captured the down, so splitting them across elements silently drops the drag.
  const resizeHandlers = useMemo(
    () => ({
      onPointerDown: (event: PointerEvent<HTMLElement>) => {
        const target = event.target;
        // Buttons and menus inside the bar own their own clicks.
        if (target instanceof HTMLElement && target.closest('button,a,[data-taskbar-control]'))
          return;
        const element = event.currentTarget;
        element.setPointerCapture(event.pointerId);
        resizeCaptureRef.current = { element, pointerId: event.pointerId };
        setResizing(true);
        event.preventDefault();
      },
      onPointerMove: (event: PointerEvent<HTMLElement>) => {
        if (!resizeCaptureRef.current) return;
        const outcome = dockHeightFromPointer(event.clientY, window.innerHeight);
        if (outcome.collapse) {
          releaseResizeCapture();
          minimize();
          return;
        }
        setHeight(outcome.height);
      },
      onPointerUp: () => releaseResizeCapture(),
      onPointerCancel: () => releaseResizeCapture(),
    }),
    [minimize, releaseResizeCapture, setHeight],
  );

  async function handleTerminalLaunch() {
    if (readOnly) return;
    if (!activeProjectId) {
      toast.error('Select a project before opening a terminal');
      return;
    }
    setTerminalBusy(true);
    try {
      const drivers = await fetchManagerDrivers();
      const driver = resolveTerminalDriver(drivers.drivers);
      if (!driver) {
        toast.error('No terminal driver installed', {
          description: 'Bare terminals need the tmux or rmux driver.',
        });
        return;
      }
      const result = await postManagerLaunch({
        project_id: activeProjectId,
        mode: driver.mode,
        harness: driver.harness,
        system_wide: launchSystemWide(driver),
      });
      openRun({ runId: result.run_id });
      await Promise.all([manager.refresh(), runs.refresh()]);
    } catch (err) {
      toast.error('Terminal launch failed', {
        description: err instanceof Error ? err.message : String(err),
      });
    } finally {
      setTerminalBusy(false);
    }
  }

  async function handleStopRun(button: TaskbarRunButton) {
    const terminal = button.kind === 'terminal';
    try {
      await postRunRelease(button.runId);
      toast.success(terminal ? 'Terminal ended' : 'Run stopped');
    } catch (err) {
      if (isRunGoneError(err)) {
        // Already gone is the outcome we wanted; fall through and tidy the tab.
        toast.info('Run already ended');
      } else {
        // The run may well still be alive — resync, but leave its tab alone.
        toast.error('Stopping run failed', {
          description: err instanceof Error ? err.message : String(err),
        });
        refresh();
        return;
      }
    }
    // Ending a terminal from its x means "this tab is done", so take the tab
    // with it. Stale buttons exist for runs that died on their own; leaving one
    // behind here would answer a deliberate close with a ghost to close again.
    // A stopped worker keeps its tab — that transcript is still worth reading.
    if (terminal) closeTab(button.tabId);
    refresh();
  }

  function handleSelectButton(button: TaskbarRunButton) {
    if (button.tabId === activeTabId && open) {
      minimize();
      return;
    }
    if (button.kind === 'stale') {
      // The run is gone; raising the button shows the missing-run panel with
      // its refresh/dismiss affordances.
      setActiveTab(button.tabId);
      openRun({ runId: button.runId });
      return;
    }
    openRun({ runId: button.runId });
  }

  // Taskbar buttons: every live manager/terminal/worker, plus tabs whose runs
  // ended (stale) so a vanished session never silently discards UI state the
  // user was looking at.
  const taskbarButtons = useMemo<TaskbarRunButton[]>(() => {
    const buttons: TaskbarRunButton[] = [];
    for (const run of projectRuns.managers) {
      buttons.push({
        tabId: run.run_id,
        runId: run.run_id,
        kind: 'manager',
        label: 'Manager',
        title: `${runTabTitle(run)} — ${run.run_id}`,
        subState: run.sub_state,
      });
    }
    projectRuns.terminals.forEach((run, index) => {
      buttons.push({
        tabId: run.run_id,
        runId: run.run_id,
        kind: 'terminal',
        label: terminalRunLabel(index, projectRuns.terminals.length),
        title: run.run_id,
        subState: run.sub_state,
      });
    });
    for (const run of projectRuns.workers) {
      buttons.push({
        tabId: run.run_id,
        runId: run.run_id,
        kind: 'worker',
        label: workerButtonLabel(run),
        title: `${runTabTitle(run)} — ${run.run_id}`,
        subState: run.sub_state,
      });
    }
    // Only flag tabs as stale once the run list has actually loaded, or every
    // restored tab would flash as dead during the first fetch.
    if (runs.data) {
      const known = new Set(buttons.map((button) => button.tabId));
      for (const tab of tabs) {
        if (known.has(tab.tabId)) continue;
        buttons.push({
          tabId: tab.tabId,
          runId: tab.runId,
          kind: 'stale',
          label: workerRunTabLabel(tab.runId, null, runLabelCache),
          title: tab.runId,
        });
      }
    }
    return buttons;
  }, [projectRuns, runLabelCache, runs.data, tabs]);

  return (
    <aside
      ref={dockRef}
      id="run-dock"
      tabIndex={-1}
      className={cn(
        'fixed inset-x-0 bottom-0 z-30 flex flex-col overscroll-contain bg-background shadow-lg',
        !resizing && 'transition-[height] duration-200 motion-reduce:transition-none',
        // Edge-to-edge on Android: the safe-area padding below keeps controls
        // clear of the navigation bar while the dock background still fills
        // down to the screen edge. box-content grows the panel by the inset so
        // content stays uncropped; a full-height dock keeps border-box and pads
        // inward within the viewport. No-op off-Android.
        open && height >= 1 ? 'box-border' : 'box-content',
      )}
      style={{
        // The taskbar is always visible; the panel adds its fraction on top.
        height: open ? `max(3rem, ${(height * 100).toFixed(2)}vh)` : '3rem',
        paddingBottom: 'var(--safe-bottom)',
        paddingLeft: 'var(--safe-left)',
        paddingRight: 'var(--safe-right)',
        ...(open && height >= 1 ? { paddingTop: 'var(--safe-top)' } : null),
      }}
      role={open && height >= 1 ? 'dialog' : undefined}
      aria-modal={open && height >= 1 ? true : undefined}
      aria-label="Run Dock"
    >
      <DockTaskbar
        open={open}
        readOnly={readOnly}
        terminalBusy={terminalBusy}
        buttons={taskbarButtons}
        activeTabId={activeTabId}
        onTerminalLaunch={() => void handleTerminalLaunch()}
        onSelect={handleSelectButton}
        onStop={(button) => void handleStopRun(button)}
        onDismiss={(button) => closeTab(button.tabId)}
        onMinimize={minimize}
        resizeHandlers={resizeHandlers}
        runningAgents={<RunningAgentsMenu projectId={activeProjectId} />}
      />
      {open ? (
        <div className="min-h-0 flex-1">
          {activeRun ? (
            <RunSurface
              run={activeRun}
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
      ) : null}
    </aside>
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
