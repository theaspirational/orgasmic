// @arch arch_MK2Q2.5
import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type PointerEvent,
  type PointerEventHandler,
} from 'react';
import {
  Check,
  Gauge,
  GripHorizontal,
  Maximize2,
  Minimize2,
  PanelBottom,
  Play,
  Power,
  Replace,
} from 'lucide-react';
import { toast } from 'sonner';

import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/popover';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs';
import {
  fetchRunRuntimeOptions,
  fetchManagerDrivers,
  isRunGoneError,
  postManagerLaunch,
  postRunInput,
  postRunRuntimeOptions,
  postRunRelease,
} from '@/lib/api';
import { isPtyTerminalDriver, runDriverTag } from '@/lib/runLabels';
import type {
  ManagerDriverProfile,
  ManagerSize,
  RuntimeModelOption,
  RuntimeOptionsCatalog,
  RuntimeProviderOption,
  RunRuntimeOptionsRequest,
  RunSummary,
  RuntimeSpeed,
} from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { ManagerChatTranscript } from './ManagerChatTranscript';
import { ManagerComposer } from './ManagerComposer';
import type { TmuxPaneConnectionState, TmuxSendKeys } from './ManagerTmuxPane';

const ManagerTmuxPane = lazy(() =>
  import('./ManagerTmuxPane').then((module) => ({ default: module.ManagerTmuxPane })),
);

const TMUX_SPLIT_KEY = 'orgasmic.manager.tmux.split';
const DEFAULT_TMUX_SPLIT = 0.7;

function readTmuxSplit(): number {
  if (typeof window === 'undefined') return DEFAULT_TMUX_SPLIT;
  const raw = window.localStorage.getItem(TMUX_SPLIT_KEY);
  const parsed = raw ? Number(raw) : DEFAULT_TMUX_SPLIT;
  if (!Number.isFinite(parsed)) return DEFAULT_TMUX_SPLIT;
  return Math.min(0.85, Math.max(0.35, parsed));
}

const LAST_DRIVER_KEY = 'orgasmic.manager.driver';
const MANAGER_RESUME_DRAFT = '/orgasmic resume';
const MANAGER_DRIVER_PREFERENCE = [
  ['acp-stdio', 'codex'],
  ['acp-stdio', 'hermes'],
  ['tmux', 'claude'],
  ['tmux', 'codex'],
  ['tmux', 'cursor-agent'],
];
const SPEED_LABELS: Record<RuntimeSpeed, string> = {
  normal: 'Normal',
  fast: 'Fast',
};

function driverKey(driver: { mode: string; harness: string }): string {
  return `${driver.mode}\u0000${driver.harness}`;
}

function readLastDriverKey(): string | null {
  if (typeof window === 'undefined') return null;
  return window.localStorage.getItem(LAST_DRIVER_KEY);
}

function writeLastDriverKey(driver: { mode: string; harness: string }): void {
  if (typeof window === 'undefined') return;
  window.localStorage.setItem(LAST_DRIVER_KEY, driverKey(driver));
}

const SYSTEM_WIDE_RMUX_KEY = 'orgasmic.manager.rmuxSystemWide';

/** Whether rmux manager sessions launch system-wide (detached from the daemon,
 * surviving restarts). Defaults ON for the manager. */
export function readRmuxSystemWide(): boolean {
  if (typeof window === 'undefined') return true;
  return window.localStorage.getItem(SYSTEM_WIDE_RMUX_KEY) !== 'false';
}

function writeRmuxSystemWide(value: boolean): void {
  if (typeof window === 'undefined') return;
  window.localStorage.setItem(SYSTEM_WIDE_RMUX_KEY, String(value));
}

/** `system_wide` value for a manager launch with this driver. Only rmux
 * sessions can detach from the daemon; other modes always send false. */
export function managerLaunchSystemWide(driver: { mode: string }): boolean {
  return driver.mode === 'rmux' && readRmuxSystemWide();
}

const LAUNCH_MODEL_KEY = 'orgasmic.manager.launchModel';

// Claude CLI model ids offered for launch-time pinning. Pinning rides the
// launch argv (`claude --model <id>`), so the choice stays scoped to that
// session — an in-session `/model` would rewrite the operator's saved
// harness-wide default.
const CLAUDE_LAUNCH_MODELS: { id: string; label: string }[] = [
  { id: 'claude-sonnet-4-6', label: 'Sonnet 4.6' },
  { id: 'claude-opus-4-8', label: 'Opus 4.8' },
  { id: 'claude-haiku-4-5', label: 'Haiku 4.5' },
  { id: 'claude-fable-5', label: 'Fable 5' },
];

function launchModelStorageKey(harness: string): string {
  return `${LAUNCH_MODEL_KEY}.${harness}`;
}

function readLaunchModel(harness: string): string | null {
  if (typeof window === 'undefined') return null;
  const value = window.localStorage.getItem(launchModelStorageKey(harness));
  return value?.trim() ? value : null;
}

function writeLaunchModel(harness: string, model: string | null): void {
  if (typeof window === 'undefined') return;
  if (model) window.localStorage.setItem(launchModelStorageKey(harness), model);
  else window.localStorage.removeItem(launchModelStorageKey(harness));
}

/** Launch-time model override for a manager launch with this driver. Only
 * claude has a stable preset vocabulary today; other harnesses launch with
 * their own defaults (undefined = no override sent). */
export function managerLaunchModel(driver: { harness: string }): string | undefined {
  if (driver.harness !== 'claude') return undefined;
  return readLaunchModel(driver.harness) ?? undefined;
}

const LAUNCH_ARGS_KEY = 'orgasmic.manager.launchArgs';

function launchArgsStorageKey(driver: { mode: string; harness: string }): string {
  return `${LAUNCH_ARGS_KEY}.${driver.mode}.${driver.harness}`;
}

function readLaunchArgs(driver: { mode: string; harness: string }): string {
  if (typeof window === 'undefined') return '';
  return window.localStorage.getItem(launchArgsStorageKey(driver)) ?? '';
}

function writeLaunchArgs(driver: { mode: string; harness: string }, value: string): void {
  if (typeof window === 'undefined') return;
  if (value.trim()) window.localStorage.setItem(launchArgsStorageKey(driver), value);
  else window.localStorage.removeItem(launchArgsStorageKey(driver));
}

/** Extra harness argv for a manager launch — the launcher's escape hatch for
 * harnesses without typed options. Only the PTY modes append free args to the
 * harness CLI; other transports spawn protocol-shaped argv. */
export function managerLaunchArgs(driver: {
  mode: string;
  harness: string;
}): string[] | undefined {
  if (driver.mode !== 'rmux' && driver.mode !== 'tmux') return undefined;
  const tokens = readLaunchArgs(driver).split(/\s+/).filter(Boolean);
  return tokens.length > 0 ? tokens : undefined;
}

function isManagerInputCapable(driver: ManagerDriverProfile): boolean {
  if (!driver.installed || driver.mode_installed === false) return false;
  // tmux and rmux both attach through the daemon's PTY bridge (rmux via
  // `rmux attach-session`), so the manager can drive either interactively.
  if (driver.mode === 'tmux' || driver.mode === 'rmux') return true;
  return driver.mode === 'acp-stdio' && (driver.harness === 'codex' || driver.harness === 'hermes');
}

type RuntimeOptionsKind = 'codex' | 'hermes';

function runtimeOptionsKind(run: RunSummary): RuntimeOptionsKind | null {
  const driver = (run.driver ?? '').replaceAll('_', '-').toLowerCase();
  const harness = (run.harness ?? '').toLowerCase();
  const isAcp =
    driver === 'acp-stdio' ||
    driver === 'acp-ws' ||
    driver === 'codex-appserver' ||
    driver === 'hermes';
  if (!isAcp) return null;
  if (harness === 'codex') return 'codex';
  if (harness === 'hermes') return 'hermes';
  return null;
}

type RuntimeSelection = {
  provider: string;
  model: string;
  effort: string;
  speed: RuntimeSpeed;
};

function selectableProviders(catalog: RuntimeOptionsCatalog | null): RuntimeProviderOption[] {
  if (!catalog?.provider_switching) return [];
  return catalog.providers.filter((provider) => provider.models.length > 0);
}

function selectableModels(
  catalog: RuntimeOptionsCatalog | null,
  providerId: string,
): RuntimeModelOption[] {
  if (!catalog) return [];
  if (!catalog.provider_switching) return catalog.models;
  return (
    catalog.providers.find((provider) => provider.id === providerId)?.models ??
    selectableProviders(catalog)[0]?.models ??
    []
  );
}

function chooseEffort(model: RuntimeModelOption | undefined, current?: string | null): string {
  const efforts = model?.reasoning_efforts ?? [];
  if (current && efforts.includes(current)) return current;
  if (model?.default_reasoning_effort && efforts.includes(model.default_reasoning_effort)) {
    return model.default_reasoning_effort;
  }
  return efforts[0] ?? '';
}

function chooseSpeed(
  model: RuntimeModelOption | undefined,
  current?: RuntimeSpeed | null,
): RuntimeSpeed {
  const speeds = model?.speeds ?? [];
  if (current && speeds.includes(current)) return current;
  if (speeds.includes('normal')) return 'normal';
  return speeds[0] ?? 'normal';
}

function initialRuntimeSelection(catalog: RuntimeOptionsCatalog): RuntimeSelection {
  const providers = selectableProviders(catalog);
  const provider =
    providers.find((option) => option.current || option.id === catalog.current.provider)?.id ??
    providers[0]?.id ??
    '';
  const models = selectableModels(catalog, provider);
  const modelOption =
    models.find((option) => option.current || option.id === catalog.current.model) ?? models[0];
  return {
    provider,
    model: modelOption?.id ?? '',
    effort: chooseEffort(modelOption, catalog.current.reasoning_effort),
    speed: chooseSpeed(modelOption, catalog.current.speed),
  };
}

function sortManagerDrivers(drivers: ManagerDriverProfile[]): ManagerDriverProfile[] {
  return [...drivers].sort((a, b) => {
    const ai = MANAGER_DRIVER_PREFERENCE.findIndex(
      ([mode, harness]) => a.mode === mode && a.harness === harness,
    );
    const bi = MANAGER_DRIVER_PREFERENCE.findIndex(
      ([mode, harness]) => b.mode === mode && b.harness === harness,
    );
    const ar = ai === -1 ? Number.MAX_SAFE_INTEGER : ai;
    const br = bi === -1 ? Number.MAX_SAFE_INTEGER : bi;
    return ar - br;
  });
}

type DriverModeGroup = {
  mode: string;
  modeLabel: string;
  providers: ManagerDriverProfile[];
};

// Group drivers by transport mode, preserving the backend's order, so both the
// launcher and the switcher present a "pick a mode, then a provider" choice
// instead of one flat list of every (mode, harness) pair.
function groupDriversByMode(drivers: ManagerDriverProfile[]): DriverModeGroup[] {
  const groups: DriverModeGroup[] = [];
  const byMode = new Map<string, DriverModeGroup>();
  for (const driver of drivers) {
    let group = byMode.get(driver.mode);
    if (!group) {
      group = { mode: driver.mode, modeLabel: driver.mode_label, providers: [] };
      byMode.set(driver.mode, group);
      groups.push(group);
    }
    group.providers.push(driver);
  }
  return groups;
}

export function ManagerWorkbench({
  size,
  projectId,
  runs,
  activeRun,
  legacyDriverTag,
  initialSource,
  initialDraft,
  tickBusy,
  hideChrome = false,
  onSelectRun,
  onSetSize,
  onTickStart,
  onTickEnd,
  onConsumeDraft,
  onPointerDown,
  onPointerMove,
  onPointerUp,
  onPointerCancel,
}: {
  size: ManagerSize;
  projectId: string | null;
  runs: RunSummary[];
  activeRun: RunSummary | null;
  legacyDriverTag: string;
  initialSource?: string | null;
  initialDraft?: string | null;
  tickBusy: boolean;
  hideChrome?: boolean;
  onSelectRun: (runId: string) => void;
  onSetSize: (size: ManagerSize) => void;
  onTickStart: () => void;
  onTickEnd: () => Promise<void>;
  onConsumeDraft?: () => void;
  onPointerDown?: PointerEventHandler<HTMLButtonElement>;
  onPointerMove?: PointerEventHandler<HTMLButtonElement>;
  onPointerUp?: PointerEventHandler<HTMLButtonElement>;
  onPointerCancel?: PointerEventHandler<HTMLButtonElement>;
}) {
  const driverId = activeRun?.driver?.trim() || legacyDriverTag;
  const driverTag = activeRun ? runDriverTag(activeRun, initialSource) : legacyDriverTag;
  const drivers = useResource('manager-drivers', fetchManagerDrivers);
  const [draftByRunId, setDraftByRunId] = useState<Record<string, string>>({});

  const clearDraftForRun = useCallback((runId: string) => {
    setDraftByRunId((prev) => {
      if (!(runId in prev)) return prev;
      const next = { ...prev };
      delete next[runId];
      return next;
    });
  }, []);

  function rememberDraft(runId: string, draft: string | null | undefined) {
    if (!draft?.trim()) return;
    setDraftByRunId((prev) => ({ ...prev, [runId]: draft }));
  }

  async function handleLaunch(driver: ManagerDriverProfile) {
    if (!projectId) {
      toast.error('Select a project before launching the manager');
      return;
    }
    onTickStart();
    try {
      writeLastDriverKey(driver);
      const result = await postManagerLaunch({
        project_id: projectId,
        mode: driver.mode,
        harness: driver.harness,
        model: managerLaunchModel(driver),
        harness_args: managerLaunchArgs(driver),
        system_wide: managerLaunchSystemWide(driver),
      });
      // A bare terminal session is not an orgasmic manager agent — don't
      // pre-fill the resume skill prompt into it.
      if (driver.harness !== 'custom') rememberDraft(result.run_id, MANAGER_RESUME_DRAFT);
      toast.success(`Launching manager: ${driver.display_name}`);
      await onTickEnd();
    } catch (err) {
      toast.error('Manager launch failed', {
        description: err instanceof Error ? err.message : String(err),
      });
      await onTickEnd();
    }
  }

  async function handleStop() {
    if (!activeRun) return;
    onTickStart();
    try {
      await postRunRelease(activeRun.run_id);
      toast.success('Manager stopped');
      await onTickEnd();
    } catch (err) {
      if (isRunGoneError(err)) {
        // The run already ended daemon-side; refreshing clears the stale pin.
        toast.info('Manager run already ended');
      } else {
        toast.error('Stopping manager failed', {
          description: err instanceof Error ? err.message : String(err),
        });
      }
      await onTickEnd();
    }
  }

  async function handleSwitch(driver: ManagerDriverProfile) {
    if (!activeRun || !projectId) return;
    onTickStart();
    try {
      // A 404 means the run already ended — proceed to the relaunch.
      await postRunRelease(activeRun.run_id).catch((err) => {
        if (!isRunGoneError(err)) throw err;
      });
      writeLastDriverKey(driver);
      const result = await postManagerLaunch({
        project_id: projectId,
        mode: driver.mode,
        harness: driver.harness,
        model: managerLaunchModel(driver),
        harness_args: managerLaunchArgs(driver),
        system_wide: managerLaunchSystemWide(driver),
      });
      if (driver.harness !== 'custom') rememberDraft(result.run_id, MANAGER_RESUME_DRAFT);
      toast.success(`Switching manager to ${driver.display_name}`);
      await onTickEnd();
    } catch (err) {
      toast.error('Switching driver failed', {
        description: err instanceof Error ? err.message : String(err),
      });
      await onTickEnd();
    }
  }

  async function handleRuntimeOptions(options: RunRuntimeOptionsRequest) {
    if (!activeRun) return;
    onTickStart();
    try {
      const response = await postRunRuntimeOptions(activeRun.run_id, options);
      if (!response.accepted) {
        throw new Error(response.message ?? 'Runtime options rejected.');
      }
      toast.success('Runtime options updated');
      await onTickEnd();
    } catch (err) {
      toast.error('Runtime options failed', {
        description: err instanceof Error ? err.message : String(err),
      });
      await onTickEnd();
      throw err;
    }
  }

  const installedDrivers = sortManagerDrivers(
    drivers.data?.drivers.filter(isManagerInputCapable) ?? [],
  );
  // A staged recovery draft handed in by the Run Dock takes precedence over any
  // locally remembered launch draft for the active run.
  const activeDraft = activeRun
    ? (initialDraft ?? draftByRunId[activeRun.run_id] ?? null)
    : null;

  function handlePromptSent(runId: string) {
    clearDraftForRun(runId);
    onConsumeDraft?.();
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      {hideChrome ? (
        activeRun ? (
          <div className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5">
            <span className="rounded-md border bg-muted px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
              {driverTag}
            </span>
            <RuntimeOptionsSwitcher
              run={activeRun}
              disabled={tickBusy}
              onApply={(options) => handleRuntimeOptions(options)}
            />
            <span className="min-w-0 flex-1" />
            <DriverSwitcher
              drivers={installedDrivers}
              disabled={tickBusy}
              onSwitch={(driver) => void handleSwitch(driver)}
            />
          </div>
        ) : null
      ) : (
        <header className="flex shrink-0 items-center gap-2 border-b px-3 py-2">
          <button
            type="button"
            className="flex h-11 w-10 touch-none items-center justify-center rounded-md text-muted-foreground hover:bg-muted md:h-7"
            aria-label="Resize manager"
            onPointerDown={onPointerDown}
            onPointerMove={onPointerMove}
            onPointerUp={onPointerUp}
            onPointerCancel={onPointerCancel}
          >
            <GripHorizontal className="size-4" />
          </button>
          <span className="rounded-md border bg-muted px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
            {driverTag}
          </span>
          {activeRun ? (
            <RuntimeOptionsSwitcher
              run={activeRun}
              disabled={tickBusy}
              onApply={(options) => handleRuntimeOptions(options)}
            />
          ) : null}
          {runs.length > 1 ? (
            <RunPicker runs={runs} activeRun={activeRun} onSelectRun={onSelectRun} />
          ) : activeRun ? (
            <span className="min-w-0 truncate font-mono text-xs text-muted-foreground">
              {activeRun.run_id}
            </span>
          ) : null}
          <div className="ml-auto flex items-center gap-1">
            {activeRun ? (
              <>
                <DriverSwitcher
                  drivers={installedDrivers}
                  disabled={tickBusy}
                  onSwitch={(driver) => void handleSwitch(driver)}
                />
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-sm"
                  aria-label="Stop manager"
                  title="Stop manager"
                  disabled={tickBusy}
                  onClick={() => void handleStop()}
                >
                  <Power />
                </Button>
                <span className="mx-1 h-5 w-px bg-border" aria-hidden />
              </>
            ) : null}
            <Button
              type="button"
              variant={size === 'peek' ? 'secondary' : 'ghost'}
              size="icon-sm"
              aria-label="Peek"
              onClick={() => onSetSize('peek')}
            >
              <PanelBottom />
            </Button>
            <Button
              type="button"
              variant={size === 'workbench' ? 'secondary' : 'ghost'}
              size="icon-sm"
              aria-label="Workbench"
              onClick={() => onSetSize('workbench')}
            >
              <Minimize2 />
            </Button>
            <Button
              type="button"
              variant={size === 'focus' ? 'secondary' : 'ghost'}
              size="icon-sm"
              aria-label="Focus"
              onClick={() => onSetSize('focus')}
            >
              <Maximize2 />
            </Button>
          </div>
        </header>
      )}
      <div className="min-h-0 flex-1">
        {activeRun ? (
          isPtyTerminalDriver(driverId) ? (
            <ManagerTmuxStack
              runId={activeRun.run_id}
              initialDraft={activeDraft}
              onPromptSent={() => handlePromptSent(activeRun.run_id)}
            />
          ) : (
            <ManagerAcpStack
              runId={activeRun.run_id}
              initialSource={initialSource}
              initialDraft={activeDraft}
              onPromptSent={() => handlePromptSent(activeRun.run_id)}
            />
          )
        ) : (
          <ManagerLauncher
            drivers={drivers.data?.drivers ?? []}
            loading={drivers.loading && !drivers.data}
            installedDrivers={installedDrivers}
            busy={tickBusy}
            onLaunch={(driver) => void handleLaunch(driver)}
          />
        )}
      </div>
    </div>
  );
}

function ManagerAcpStack({
  runId,
  initialSource,
  initialDraft,
  onPromptSent,
}: {
  runId: string;
  initialSource?: string | null;
  initialDraft?: string | null;
  onPromptSent: () => void;
}) {
  const [pendingSince, setPendingSince] = useState<string | null>(null);

  useEffect(() => {
    setPendingSince(null);
  }, [runId]);

  async function handleSend(text: string): Promise<boolean> {
    const response = await postRunInput(runId, text);
    if (!response.accepted) {
      throw new Error(response.message ?? 'Manager rejected input.');
    }
    return true;
  }

  const handleSent = useCallback(
    (sentAt: string) => {
      setPendingSince(sentAt);
      onPromptSent();
    },
    [onPromptSent],
  );

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="min-h-0 flex-1">
        <ManagerChatTranscript
          runId={runId}
          initialSource={initialSource}
          pendingSince={pendingSince}
          onPendingResolved={() => setPendingSince(null)}
        />
      </div>
      <div className="min-h-[132px] border-t">
        <ManagerComposer
          runId={runId}
          connectionState="open"
          initialDraft={initialDraft}
          placeholder="Send to agent"
          readyLabel="Enter starts the turn. Shift+Enter adds a line. Arrow-up recalls the last send."
          onSend={handleSend}
          onSent={handleSent}
        />
      </div>
    </div>
  );
}

function ManagerTmuxStack({
  runId,
  initialDraft,
  onPromptSent,
}: {
  runId: string;
  initialDraft?: string | null;
  onPromptSent: () => void;
}) {
  const [split, setSplit] = useState(readTmuxSplit);
  const [connState, setConnState] = useState<TmuxPaneConnectionState>('connecting');
  const sendRef = useRef<TmuxSendKeys | null>(null);
  const stackRef = useRef<HTMLDivElement | null>(null);
  const dragRef = useRef<{ pointerId: number; element: HTMLButtonElement } | null>(null);

  const setPersistedSplit = useCallback((next: number) => {
    const clamped = Math.min(0.85, Math.max(0.35, next));
    setSplit(clamped);
    window.localStorage.setItem(TMUX_SPLIT_KEY, String(clamped));
  }, []);
  const handleSendReady = useCallback((send: TmuxSendKeys | null) => {
    sendRef.current = send;
  }, []);

  const updateSplitFromPointer = useCallback(
    (clientY: number) => {
      const rect = stackRef.current?.getBoundingClientRect();
      if (!rect || rect.height <= 0) return;
      setPersistedSplit((clientY - rect.top) / rect.height);
    },
    [setPersistedSplit],
  );

  const finishSplitDrag = useCallback(
    (event: PointerEvent<HTMLButtonElement>) => {
      const drag = dragRef.current;
      if (!drag) return;
      try {
        if (drag.element.hasPointerCapture(drag.pointerId)) {
          drag.element.releasePointerCapture(drag.pointerId);
        }
      } catch {
        /* Pointer capture may be gone after cancellation. */
      }
      dragRef.current = null;
      updateSplitFromPointer(event.clientY);
    },
    [updateSplitFromPointer],
  );

  useEffect(() => {
    return () => {
      const drag = dragRef.current;
      if (!drag) return;
      try {
        if (drag.element.hasPointerCapture(drag.pointerId)) {
          drag.element.releasePointerCapture(drag.pointerId);
        }
      } catch {
        /* Interrupted pointer captures may already be gone. */
      }
      dragRef.current = null;
    };
  }, []);

  function handleSplitPointerDown(event: PointerEvent<HTMLButtonElement>) {
    if (event.button !== 0) return;
    dragRef.current = { pointerId: event.pointerId, element: event.currentTarget };
    event.currentTarget.setPointerCapture(event.pointerId);
    updateSplitFromPointer(event.clientY);
  }

  function handleSplitPointerMove(event: PointerEvent<HTMLButtonElement>) {
    if (!dragRef.current) return;
    // pointerup can be dropped over xterm or outside the window; buttons still clears.
    if ((event.buttons & 1) === 0) {
      finishSplitDrag(event);
      return;
    }
    updateSplitFromPointer(event.clientY);
  }

  function handleSplitLostPointerCapture(event: PointerEvent<HTMLButtonElement>) {
    if (!dragRef.current || dragRef.current.pointerId !== event.pointerId) return;
    dragRef.current = null;
  }

  return (
    <div ref={stackRef} className="flex h-full min-h-0 flex-col">
      <div className="min-h-0" style={{ flexBasis: `${split * 100}%` }}>
        <Suspense
          fallback={
            <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
              Loading terminal...
            </div>
          }
        >
          <ManagerTmuxPane
            runId={runId}
            onConnectionState={setConnState}
            onSendReady={handleSendReady}
          />
        </Suspense>
      </div>
      <button
        type="button"
        className="group flex h-3 shrink-0 touch-none items-center justify-center border-y bg-muted/30 hover:bg-muted"
        aria-label="Resize terminal composer split"
        onPointerDown={handleSplitPointerDown}
        onPointerMove={handleSplitPointerMove}
        onPointerUp={finishSplitDrag}
        onPointerCancel={finishSplitDrag}
        onLostPointerCapture={handleSplitLostPointerCapture}
      >
        <span className="h-1 w-10 rounded-full bg-border group-hover:bg-muted-foreground/60" />
      </button>
      <div className="min-h-[116px] flex-1">
        <ManagerComposer
          runId={runId}
          connectionState={connState}
          initialDraft={initialDraft}
          placeholder="Send to agent"
          readyLabel="Enter sends to the terminal. Shift+Enter adds a line. Arrow-up recalls the last send."
          unavailableLabel="No tmux terminal attached."
          onSend={(text) => sendRef.current?.(text) ?? false}
          onSent={onPromptSent}
        />
      </div>
    </div>
  );
}

// Driver-adaptive launcher (dec_029): with no active manager run the workbench
// leads with a "choose driver, then start" surface. Only drivers whose CLI
// binary is on PATH can be started.
function ManagerLauncher({
  drivers,
  installedDrivers,
  loading,
  busy,
  onLaunch,
}: {
  drivers: ManagerDriverProfile[];
  installedDrivers: ManagerDriverProfile[];
  loading: boolean;
  busy: boolean;
  onLaunch: (driver: ManagerDriverProfile) => void;
}) {
  if (loading) {
    return (
      <div className="shrink-0 border-b p-3">
        <Skeleton className="h-9 w-full" />
      </div>
    );
  }

  if (drivers.length === 0) {
    return (
      <div className="shrink-0 border-b p-3">
        <p className="text-center text-xs text-muted-foreground">No manager drivers available.</p>
      </div>
    );
  }

  if (installedDrivers.length === 0) {
    return (
      <div className="shrink-0 border-b p-3 text-center text-xs text-muted-foreground">
        <p>No supported CLI on PATH.</p>
        <p className="mt-1">
          Install one of:{' '}
          {[...new Set(drivers.map((d) => d.binary))].map((binary, i) => (
            <span key={binary}>
              {i > 0 ? ', ' : ''}
              <code className="rounded bg-muted px-1 py-0.5 font-mono">{binary}</code>
            </span>
          ))}
          .
        </p>
      </div>
    );
  }

  return (
    <DriverChooser
      installedDrivers={installedDrivers}
      busy={busy}
      onLaunch={onLaunch}
    />
  );
}

// Two-tier driver chooser: a segmented control picks the transport mode, then a
// row of provider buttons launches under that mode. Keeps the footer scannable
// as more (mode, harness) pairs land, instead of a flat wall of every combo.
function DriverChooser({
  installedDrivers,
  busy,
  onLaunch,
}: {
  installedDrivers: ManagerDriverProfile[];
  busy: boolean;
  onLaunch: (driver: ManagerDriverProfile) => void;
}) {
  const groups = groupDriversByMode(installedDrivers);
  const lastKey = readLastDriverKey();
  const lastDriver = installedDrivers.find((d) => driverKey(d) === lastKey) ?? null;
  // Reopen on the last-used transport; fall back to the first available mode.
  const [selectedMode, setSelectedMode] = useState(() => lastDriver?.mode ?? groups[0]?.mode ?? '');
  const activeMode = groups.some((g) => g.mode === selectedMode)
    ? selectedMode
    : (groups[0]?.mode ?? '');
  const [systemWide, setSystemWide] = useState(readRmuxSystemWide);

  function toggleSystemWide(value: boolean) {
    setSystemWide(value);
    writeRmuxSystemWide(value);
  }

  // Harness selection mirrors the mode row: a second segmented Tabs strip.
  // Remembered from the last launch; falls back to the group's first harness
  // when the active mode has no tab with the remembered id.
  const [selectedHarness, setSelectedHarness] = useState(() => lastDriver?.harness ?? '');
  const activeGroup = groups.find((group) => group.mode === activeMode) ?? null;
  const activeDriver =
    activeGroup?.providers.find((driver) => driver.harness === selectedHarness) ??
    activeGroup?.providers[0] ??
    null;

  return (
    <div className="flex shrink-0 flex-col gap-2.5 border-b p-3">
      <span className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
        Launch manager
      </span>
      <div className="flex flex-wrap items-center gap-2">
        {groups.length > 1 ? (
          <Tabs value={activeMode} onValueChange={setSelectedMode}>
            <TabsList className="h-7">
              {groups.map((group) => (
                <TabsTrigger key={group.mode} value={group.mode} className="px-2.5 text-xs">
                  {group.modeLabel}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        ) : (
          <span className="font-mono text-[11px] text-muted-foreground">
            {groups[0]?.modeLabel}
          </span>
        )}
        {activeGroup ? (
          <Tabs value={activeDriver?.harness ?? ''} onValueChange={setSelectedHarness}>
            <TabsList className="h-7">
              {activeGroup.providers.map((driver) => (
                <TabsTrigger
                  key={driver.harness}
                  value={driver.harness}
                  className="px-2.5 text-xs"
                >
                  {driver.harness_label}
                </TabsTrigger>
              ))}
            </TabsList>
          </Tabs>
        ) : null}
      </div>
      {activeDriver ? <HarnessLaunchOptions driver={activeDriver} busy={busy} /> : null}
      {activeMode === 'rmux' ? (
        <label className="flex cursor-pointer items-center gap-1.5 text-[11px] text-muted-foreground">
          <input
            type="checkbox"
            className="size-3.5 accent-primary"
            checked={systemWide}
            disabled={busy}
            onChange={(e) => toggleSystemWide(e.target.checked)}
          />
          System-wide session (survives daemon restart)
        </label>
      ) : null}
      <div>
        <Button
          type="button"
          size="sm"
          disabled={busy || !activeDriver}
          onClick={() => activeDriver && onLaunch(activeDriver)}
        >
          <Play className="size-4" />
          Launch {activeDriver?.harness_label ?? ''}
        </Button>
      </div>
    </div>
  );
}

// Per-harness launch options: a typed selector where the harness has a stable
// vocabulary (claude → model presets), a free-args escape hatch for the other
// PTY harnesses, and a hint for the bare-terminal pseudo-harness.
function HarnessLaunchOptions({
  driver,
  busy,
}: {
  driver: ManagerDriverProfile;
  busy: boolean;
}) {
  if (driver.harness === 'claude') return <LaunchModelSelect busy={busy} />;
  if (driver.harness === 'custom') {
    return (
      <p className="text-[11px] leading-snug text-muted-foreground">
        Bare terminal session — no agent CLI. Run any tool yourself in the
        attached terminal (e.g. a harness orgasmic doesn&apos;t support natively yet).
      </p>
    );
  }
  if (isPtyTerminalDriver(driver.mode)) return <LaunchArgsInput driver={driver} busy={busy} />;
  return null;
}

const LAUNCH_MODEL_DEFAULT = 'default';

// Launch-time model choice for claude launches. Pinned via the launch argv so
// it stays session-scoped; "Harness default" sends no override at all.
function LaunchModelSelect({ busy }: { busy: boolean }) {
  const [model, setModel] = useState(() => readLaunchModel('claude') ?? LAUNCH_MODEL_DEFAULT);

  function handleChange(value: string) {
    setModel(value);
    writeLaunchModel('claude', value === LAUNCH_MODEL_DEFAULT ? null : value);
  }

  return (
    <label className="mt-2 flex items-center gap-1.5 text-[11px] text-muted-foreground">
      <span className="font-medium uppercase tracking-wide">Claude model</span>
      <Select value={model} onValueChange={handleChange} disabled={busy}>
        <SelectTrigger size="sm" className="h-7 w-44 text-xs">
          <SelectValue />
        </SelectTrigger>
        <SelectContent align="start">
          <SelectItem value={LAUNCH_MODEL_DEFAULT}>Harness default</SelectItem>
          {CLAUDE_LAUNCH_MODELS.map((option) => (
            <SelectItem key={option.id} value={option.id}>
              {option.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </label>
  );
}

// Free-args escape hatch for PTY harnesses without typed launch options. The
// value is persisted per (mode, harness) and split on whitespace into the
// argv appended to the harness CLI.
function LaunchArgsInput({
  driver,
  busy,
}: {
  driver: ManagerDriverProfile;
  busy: boolean;
}) {
  const storageKey = launchArgsStorageKey(driver);
  const [value, setValue] = useState(() => readLaunchArgs(driver));

  // Each (mode, harness) keeps its own remembered args.
  useEffect(() => {
    setValue(readLaunchArgs(driver));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [storageKey]);

  return (
    <label className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
      <span className="shrink-0 font-medium uppercase tracking-wide">Extra args</span>
      <Input
        className="h-7 w-72 font-mono text-xs"
        placeholder={`passed to ${driver.binary} verbatim, e.g. --model x`}
        value={value}
        disabled={busy}
        onChange={(e) => {
          setValue(e.target.value);
          writeLaunchArgs(driver, e.target.value);
        }}
      />
    </label>
  );
}

function RuntimeOptionsSwitcher({
  run,
  disabled,
  onApply,
}: {
  run: RunSummary;
  disabled: boolean;
  onApply: (options: RunRuntimeOptionsRequest) => Promise<void>;
}) {
  const kind = runtimeOptionsKind(run);
  const [open, setOpen] = useState(false);
  const [catalog, setCatalog] = useState<RuntimeOptionsCatalog | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [loadingCatalog, setLoadingCatalog] = useState(false);
  const [provider, setProvider] = useState('');
  const [model, setModel] = useState('');
  const [effort, setEffort] = useState('');
  const [speed, setSpeed] = useState<RuntimeSpeed>('normal');
  const [submitting, setSubmitting] = useState(false);

  const reset = useCallback(() => {
    setCatalog(null);
    setCatalogError(null);
    setLoadingCatalog(false);
    setProvider('');
    setModel('');
    setEffort('');
    setSpeed('normal');
  }, []);

  useEffect(() => {
    reset();
  }, [reset, run.run_id]);

  useEffect(() => {
    if (!open || !kind) return;
    let cancelled = false;
    setLoadingCatalog(true);
    setCatalogError(null);
    fetchRunRuntimeOptions(run.run_id)
      .then((response) => {
        if (cancelled) return;
        const nextCatalog = response.catalog;
        const selection = initialRuntimeSelection(nextCatalog);
        setCatalog(nextCatalog);
        setProvider(selection.provider);
        setModel(selection.model);
        setEffort(selection.effort);
        setSpeed(selection.speed);
      })
      .catch((err) => {
        if (cancelled) return;
        setCatalog(null);
        setCatalogError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!cancelled) setLoadingCatalog(false);
      });
    return () => {
      cancelled = true;
    };
  }, [kind, open, run.run_id]);

  const providerOptions = useMemo(() => selectableProviders(catalog), [catalog]);
  const modelOptions = useMemo(() => selectableModels(catalog, provider), [catalog, provider]);
  const selectedModel = modelOptions.find((option) => option.id === model);
  const effortOptions = selectedModel?.reasoning_efforts ?? [];
  const speedOptions = selectedModel?.speeds ?? [];

  function updateModelSelection(nextModelId: string) {
    const nextModel = modelOptions.find((option) => option.id === nextModelId);
    setModel(nextModelId);
    setEffort(chooseEffort(nextModel, catalog?.current.reasoning_effort));
    setSpeed(chooseSpeed(nextModel, catalog?.current.speed));
  }

  function updateProviderSelection(nextProviderId: string) {
    if (!catalog) return;
    const nextModels = selectableModels(catalog, nextProviderId);
    const nextModel =
      nextModels.find((option) => option.current || option.id === catalog.current.model) ??
      nextModels[0];
    setProvider(nextProviderId);
    setModel(nextModel?.id ?? '');
    setEffort(chooseEffort(nextModel, catalog.current.reasoning_effort));
    setSpeed(chooseSpeed(nextModel, catalog.current.speed));
  }

  async function handleApply() {
    if (!catalog || !selectedModel) {
      toast.error('No runtime options are available');
      return;
    }
    const next: RunRuntimeOptionsRequest = {};
    const providerChanged = provider !== (catalog.current.provider ?? '');
    const modelChanged = model !== (catalog.current.model ?? '');
    if (catalog.provider_switching && provider && (providerChanged || modelChanged)) {
      next.provider = provider;
    }
    if (model && modelChanged) next.model = model;
    if (effort && effort !== (catalog.current.reasoning_effort ?? '')) {
      next.reasoning_effort = effort;
    }
    if (speed && speed !== catalog.current.speed) next.speed = speed;
    if (Object.keys(next).length === 0) {
      toast.info('Runtime options already selected');
      return;
    }
    setSubmitting(true);
    try {
      await onApply(next);
      setOpen(false);
      reset();
    } finally {
      setSubmitting(false);
    }
  }

  const busy = disabled || submitting || loadingCatalog;
  const canApply =
    Boolean(catalog && selectedModel && effort && speedOptions.includes(speed)) &&
    !busy &&
    !catalogError;
  const runtimeLabel = kind === 'hermes' ? 'Hermes' : 'Codex';

  if (!kind) return null;

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Runtime options"
          title="Runtime options"
          disabled={disabled}
        >
          <Gauge />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-[300px] p-3">
        <div className="flex items-center justify-between gap-2">
          <span className="text-xs font-medium">Runtime</span>
          <span className="rounded-md border bg-muted px-1.5 py-0.5 font-mono text-[10px] text-muted-foreground">
            {runtimeLabel}
          </span>
        </div>
        {loadingCatalog ? (
          <div className="mt-3 grid gap-2">
            <Skeleton className="h-7 w-full" />
            <Skeleton className="h-7 w-full" />
            <Skeleton className="h-7 w-2/3" />
          </div>
        ) : catalogError ? (
          <div className="mt-3 rounded-md border bg-muted px-2 py-2 text-xs text-muted-foreground">
            Catalog unavailable.
          </div>
        ) : modelOptions.length === 0 ? (
          <div className="mt-3 rounded-md border bg-muted px-2 py-2 text-xs text-muted-foreground">
            No runtime options discovered.
          </div>
        ) : (
          <div className="mt-3 grid gap-2.5">
            {catalog?.provider_switching && providerOptions.length > 0 ? (
              <label className="grid gap-1.5">
                <span className="text-[11px] font-medium uppercase text-muted-foreground">
                  Provider
                </span>
                <Select
                  value={provider}
                  disabled={busy}
                  onValueChange={updateProviderSelection}
                >
                  <SelectTrigger size="sm" className="w-full">
                    <SelectValue placeholder="Provider" />
                  </SelectTrigger>
                  <SelectContent align="start">
                    {providerOptions.map((option) => (
                      <SelectItem key={option.id} value={option.id}>
                        {option.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </label>
            ) : null}
            <label className="grid gap-1.5">
              <span className="text-[11px] font-medium uppercase text-muted-foreground">Model</span>
              <Select value={model} disabled={busy} onValueChange={updateModelSelection}>
                <SelectTrigger size="sm" className="w-full">
                  <SelectValue placeholder="Model" />
                </SelectTrigger>
                <SelectContent align="start">
                  {modelOptions.map((option) => (
                    <SelectItem key={`${option.provider ?? 'codex'}:${option.id}`} value={option.id}>
                      {option.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </label>
            {effortOptions.length > 0 ? (
              <label className="grid gap-1.5">
                <span className="text-[11px] font-medium uppercase text-muted-foreground">
                  Effort
                </span>
                <Select value={effort} disabled={busy} onValueChange={setEffort}>
                  <SelectTrigger size="sm" className="w-full">
                    <SelectValue placeholder="Effort" />
                  </SelectTrigger>
                  <SelectContent align="start">
                    {effortOptions.map((option) => (
                      <SelectItem key={option} value={option}>
                        {option}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </label>
            ) : null}
            <div className="grid gap-1.5">
              <span className="text-[11px] font-medium uppercase text-muted-foreground">Speed</span>
              <div
                className="grid gap-1"
                style={{
                  gridTemplateColumns: `repeat(${Math.max(speedOptions.length, 1)}, minmax(0, 1fr))`,
                }}
              >
                {speedOptions.map((option) => (
                  <Button
                    key={option}
                    type="button"
                    variant={speed === option ? 'secondary' : 'outline'}
                    size="sm"
                    className="h-7 px-2 text-xs"
                    disabled={busy}
                    onClick={() => setSpeed(option)}
                  >
                    {SPEED_LABELS[option]}
                  </Button>
                ))}
              </div>
            </div>
          </div>
        )}
        <div className="mt-3 flex justify-end">
          <Button type="button" size="sm" disabled={!canApply} onClick={() => void handleApply()}>
            <Check className="size-4" />
            Apply
          </Button>
        </div>
      </PopoverContent>
    </Popover>
  );
}

// Live driver switch on a running manager: release the current run, then
// re-launch under the chosen driver (orgasmic has no in-place transport swap).
function DriverSwitcher({
  drivers,
  disabled,
  onSwitch,
}: {
  drivers: ManagerDriverProfile[];
  disabled: boolean;
  onSwitch: (driver: ManagerDriverProfile) => void;
}) {
  const [launchModel, setLaunchModel] = useState(
    () => readLaunchModel('claude') ?? LAUNCH_MODEL_DEFAULT,
  );
  if (drivers.length === 0) return null;
  const groups = groupDriversByMode(drivers);
  const hasClaude = drivers.some((driver) => driver.harness === 'claude');

  function handleModelPick(value: string) {
    setLaunchModel(value);
    writeLaunchModel('claude', value === LAUNCH_MODEL_DEFAULT ? null : value);
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label="Switch manager driver"
          title="Switch driver (stops and relaunches)"
          disabled={disabled}
        >
          <Replace />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-52">
        <DropdownMenuLabel>Relaunch under…</DropdownMenuLabel>
        <DropdownMenuSeparator />
        {groups.map((group) =>
          // A mode with a single provider has no second choice to make — flatten
          // it to a direct item rather than burying it behind a submenu.
          group.providers.length === 1 ? (
            <DropdownMenuItem key={group.mode} onClick={() => onSwitch(group.providers[0]!)}>
              <span className="flex-1">{group.providers[0]!.harness_label}</span>
              <span className="font-mono text-[10px] text-muted-foreground">{group.modeLabel}</span>
            </DropdownMenuItem>
          ) : (
            <DropdownMenuSub key={group.mode}>
              <DropdownMenuSubTrigger>{group.modeLabel}</DropdownMenuSubTrigger>
              <DropdownMenuSubContent>
                {group.providers.map((driver) => (
                  <DropdownMenuItem key={driverKey(driver)} onClick={() => onSwitch(driver)}>
                    {driver.harness_label}
                  </DropdownMenuItem>
                ))}
              </DropdownMenuSubContent>
            </DropdownMenuSub>
          ),
        )}
        {hasClaude ? (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuSub>
              <DropdownMenuSubTrigger>
                <span className="flex-1">Claude model</span>
                <span className="font-mono text-[10px] text-muted-foreground">
                  {CLAUDE_LAUNCH_MODELS.find((option) => option.id === launchModel)?.label ??
                    'default'}
                </span>
              </DropdownMenuSubTrigger>
              <DropdownMenuSubContent>
                {[{ id: LAUNCH_MODEL_DEFAULT, label: 'Harness default' }, ...CLAUDE_LAUNCH_MODELS].map(
                  (option) => (
                    <DropdownMenuItem key={option.id} onClick={() => handleModelPick(option.id)}>
                      <span className="flex-1">{option.label}</span>
                      {launchModel === option.id ? <Check className="size-3.5" /> : null}
                    </DropdownMenuItem>
                  ),
                )}
              </DropdownMenuSubContent>
            </DropdownMenuSub>
          </>
        ) : null}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function RunPicker({
  runs,
  activeRun,
  onSelectRun,
}: {
  runs: RunSummary[];
  activeRun: RunSummary | null;
  onSelectRun: (runId: string) => void;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button type="button" variant="outline" size="sm" className="min-w-0 max-w-[220px]">
          <span className="truncate font-mono">{activeRun?.run_id ?? 'Select run'}</span>
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-72">
        {runs.map((run) => (
          <DropdownMenuItem key={run.run_id} onClick={() => onSelectRun(run.run_id)}>
            <span className="min-w-0 flex-1 truncate font-mono">{run.run_id}</span>
            {run.sub_state ? (
              <span className="font-mono text-[10px] text-muted-foreground">{run.sub_state}</span>
            ) : null}
            <span className="text-xs text-muted-foreground">{run.event_count}</span>
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
