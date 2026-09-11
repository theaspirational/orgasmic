import {
  lazy,
  Suspense,
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
  fetchLiveRuns,
  isRunGoneError,
  postManagerLaunch,
  postRunRelease,
} from '@/lib/api';
import { useContainedWheelRef } from '@/lib/containedWheel';
import { CHAT_TAB_ID, useRunDock } from '@/lib/runDock';
import {
  dockHeightFromDrag,
  DOCK_DRAG_THRESHOLD_PX,
  MAX_DOCK_HEIGHT,
} from '@/lib/runDockUtils';
import { runTabTitle } from '@/lib/runLabels';
import type { DaemonEvent, RunSummary } from '@/lib/types';
import { cn } from '@/lib/utils';
import { useResource } from '@/lib/useResource';

import { DockTaskbar, type TaskbarRunButton } from './DockTaskbar';
import { RunningAgentsMenu } from './RunningAgentsMenu';
import {
  isTerminalRun,
  taskbarRunGroups,
  terminalRunLabel,
  workerButtonLabel,
  workerRunTabLabel,
} from './runDockLabels';
import { resolveTerminalDriver } from './terminalLaunch';

const FinishedRunPanel = lazy(() =>
  import('./ConversationPanel').then((module) => ({ default: module.FinishedRunPanel })),
);
const RunSurface = lazy(() =>
  import('./RunSurface').then((module) => ({ default: module.RunSurface })),
);
// Lazy for the same reason as the run surface: the conversation panel renders
// the same transcript, and with it the markdown renderer and mermaid.
const ConversationPanel = lazy(() =>
  import('./ConversationPanel').then((module) => ({ default: module.ConversationPanel })),
);

export function RunDock() {
  const { activeProjectId } = useActiveProject();
  const { can, isMember } = useMe();
  // The dock only renders when the viewer may watch sessions (see AppShell). A
  // member who can watch but lacks sessions.interact gets a read-only surface:
  // no composer, no PTY input, no launch/stop. Admin ⇒ can() true ⇒ interactive.
  const readOnly = !can(activeProjectId, 'sessions.interact');
  // Conversations are nodes; seeing them is chat.read (CHAT-SCOPE §6).
  const chatEnabled = can(activeProjectId, 'chat.read');
  const {
    open,
    height,
    tabs,
    activeTabId,
    setHeight,
    setActiveTab,
    openRun,
    openChat,
    replaceLiveRuns,
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
  const resizeCaptureRef = useRef<{
    element: HTMLElement;
    pointerId: number;
    startY: number;
    startHeight: number;
    dragging: boolean;
  } | null>(null);

  // Admin-only route; a member mounted for chat (AppShell) must not poll it.
  const manager = useResource('rundock-manager-state', fetchManagerState, { enabled: !isMember });
  // Run buttons need summaries (driver/kind/task) to label and render.
  // orgasmic:task_6HJYT — the dock renders live runs only, so it reads the
  // supervisor-local live source rather than the recovery inventory. Tab
  // restore (lib/runDock.tsx) is the path that genuinely needs recovered
  // classifications and stays on the inventory.
  const runs = useResource('rundock-runs', fetchLiveRuns);

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

  // Live runs with the manager snapshot folded in (see runById): a conversation
  // created a moment ago is already findable by its lease key here.
  const knownRuns = useMemo(() => [...runById.values()], [runById]);

  useEffect(() => {
    replaceLiveRuns(knownRuns);
  }, [replaceLiveRuns, knownRuns]);

  // Every live run of the active project earns a taskbar button the moment it
  // is dispatched — the Windows-taskbar model. Only clicking a button opens a
  // surface, so dispatch stays quiet (dec_FBBT2's noise concern holds: buttons
  // appear, sessions never raise themselves).
  const projectRuns = useMemo(() => {
    const inProject = (run: RunSummary) =>
      !activeProjectId || !run.project_id || run.project_id === activeProjectId;
    // Managers and terminals share the manager.launch namespace; the `custom`
    // pseudo-harness is what separates a bare terminal from an agent manager.
    // Presence-only external managers are centrally ineligible for the dock.
    return taskbarRunGroups([...runById.values()].filter(inProject));
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
  const chatActive = activeTabId === CHAT_TAB_ID && open && chatEnabled;
  // A run tab whose run has ended hands over to its conversation (CHAT-SCOPE
  // C2): the transcript stays, and the composer continues under the C2 rules
  // (resume, or refuse with "dispatch a new attempt").
  const handOverToConversation = useCallback(
    (conversationId: string) => {
      openChat({ conversationId });
      if (activeTabId) closeTab(activeTabId);
    },
    [activeTabId, closeTab, openChat],
  );
  const maximized = height >= MAX_DOCK_HEIGHT;

  const raiseLastTab = useCallback(() => {
    if (activeTabId === CHAT_TAB_ID) {
      openChat();
      return;
    }
    const target = activeTab ?? tabs[tabs.length - 1];
    if (target) openRun({ runId: target.runId });
    else openChat();
  }, [activeTab, activeTabId, openChat, openRun, tabs]);

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
        resizeCaptureRef.current = {
          element,
          pointerId: event.pointerId,
          startY: event.clientY,
          startHeight: height,
          dragging: false,
        };
        event.preventDefault();
      },
      onPointerMove: (event: PointerEvent<HTMLElement>) => {
        const capture = resizeCaptureRef.current;
        if (!capture) return;
        const delta = event.clientY - capture.startY;
        // Below the threshold this is still a tap; resizing here would move the
        // dock a little on every touch of the bar.
        if (!capture.dragging) {
          if (Math.abs(delta) < DOCK_DRAG_THRESHOLD_PX) return;
          capture.dragging = true;
          setResizing(true);
        }
        const outcome = dockHeightFromDrag(capture.startHeight, delta, window.innerHeight);
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
    [height, minimize, releaseResizeCapture, setHeight],
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
          description: 'Bare terminals need the tmux driver.',
        });
        return;
      }
      const result = await postManagerLaunch({
        project_id: activeProjectId,
        mode: driver.mode,
        harness: driver.harness,
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
      const known = new Set([
        ...buttons.map((button) => button.tabId),
        ...projectRuns.managers.map((run) => run.run_id),
        ...projectRuns.conversations.map((run) => run.run_id),
      ]);
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
        open && maximized ? 'box-border' : 'box-content',
      )}
      style={{
        // The taskbar is always visible; the panel adds its fraction on top.
        height: open ? `max(3rem, ${(height * 100).toFixed(2)}dvh)` : '3rem',
        paddingBottom: 'var(--safe-bottom)',
        paddingLeft: 'var(--safe-left)',
        paddingRight: 'var(--safe-right)',
        ...(open && maximized ? { paddingTop: 'var(--safe-top)' } : null),
      }}
      role={open && maximized ? 'dialog' : undefined}
      aria-modal={open && maximized ? true : undefined}
      aria-label="Run Dock"
    >
      <DockTaskbar
        open={open}
        readOnly={readOnly}
        terminalBusy={terminalBusy}
        chatActive={chatActive}
        chatVisible={chatEnabled}
        buttons={taskbarButtons}
        activeTabId={activeTabId}
        maximized={maximized}
        onTerminalLaunch={() => void handleTerminalLaunch()}
        onChatOpen={() => {
          if (chatActive) minimize();
          else openChat();
        }}
        onSelect={handleSelectButton}
        onStop={(button) => void handleStopRun(button)}
        onDismiss={(button) => closeTab(button.tabId)}
        onMaximize={() => setHeight(MAX_DOCK_HEIGHT)}
        onMinimize={minimize}
        onRestore={raiseLastTab}
        resizeHandlers={resizeHandlers}
        runningAgents={<RunningAgentsMenu projectId={activeProjectId} />}
      />
      {open ? (
        <div className="min-h-0 flex-1 overflow-hidden">
          {activeTabId === CHAT_TAB_ID ? (
            chatEnabled ? (
              <Suspense fallback={null}>
                <ConversationPanel
                  key={activeProjectId ?? 'no-project'}
                  projectId={activeProjectId}
                  liveRuns={knownRuns}
                  onRefresh={refresh}
                />
              </Suspense>
            ) : (
              <p className="p-6 text-center text-sm text-muted-foreground">
                Chat requires the chat.read grant for this project.
              </p>
            )
          ) : activeRun ? (
            <Suspense fallback={null}>
              <RunSurface
                run={activeRun}
                initialDraft={activeTab?.draftPrompt}
                onPromptSent={() => activeTab && consumeDraft(activeTab.tabId)}
                readOnly={readOnly}
              />
            </Suspense>
          ) : activeTab && runs.data ? (
            <Suspense fallback={null}>
              <FinishedRunPanel
                key={activeTab.runId}
                runId={activeTab.runId}
                onRefresh={refresh}
                onClose={() => closeTab(activeTab.tabId)}
                onConversation={chatEnabled ? handOverToConversation : undefined}
              />
            </Suspense>
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
    <div className="flex h-full flex-col items-center gap-3 overflow-y-auto p-6 text-center [justify-content:safe_center]">
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
