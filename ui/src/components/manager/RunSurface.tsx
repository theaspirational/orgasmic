import { lazy, Suspense, useCallback, useEffect, useRef, useState, type PointerEvent } from 'react';

import { postRunInput } from '@/lib/api';
import { runUsesPtyTerminal } from '@/lib/runLabels';
import type { RunSummary } from '@/lib/types';

import { ManagerChatTranscript } from './ManagerChatTranscript';
import { ManagerComposer } from './ManagerComposer';
import type { TmuxPaneConnectionState, TmuxSendKeys } from './ManagerTmuxPane';
import { ReadOnlySessionBar } from './ReadOnlySessionBar';

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

// Shared transcript/terminal/composer surface for any run — manager or worker.
// The Run Dock renders one of these per open tab; the only difference between a
// manager and worker tab is provider-neutral composer copy and the role label
// the dock supplies above this component.
export function RunSurface({
  run,
  initialSource,
  initialDraft,
  onPromptSent,
  readOnly = false,
}: {
  run: RunSummary;
  initialSource?: string | null;
  initialDraft?: string | null;
  onPromptSent: () => void;
  /** Members without sessions.interact watch the stream but cannot send. */
  readOnly?: boolean;
}) {
  if (runUsesPtyTerminal(run)) {
    return (
      <RunTmuxStack
        runId={run.run_id}
        initialDraft={initialDraft}
        onPromptSent={onPromptSent}
        readOnly={readOnly}
      />
    );
  }
  return (
    <RunChatStack
      runId={run.run_id}
      initialSource={initialSource}
      initialDraft={initialDraft}
      onPromptSent={onPromptSent}
      readOnly={readOnly}
    />
  );
}

function RunChatStack({
  runId,
  initialSource,
  initialDraft,
  onPromptSent,
  readOnly,
}: {
  runId: string;
  initialSource?: string | null;
  initialDraft?: string | null;
  onPromptSent: () => void;
  readOnly: boolean;
}) {
  const [pendingSince, setPendingSince] = useState<string | null>(null);

  useEffect(() => {
    setPendingSince(null);
  }, [runId]);

  async function handleSend(text: string): Promise<boolean> {
    // Chat-stack send. POST /runs/:id/input delivers to the driver and the
    // daemon records a composer_send lifecycle event (TASK-102 / dec_052).
    // The tmux stack reaches the same recording path via the WS send_keys
    // bridge (see RunTmuxStack / ManagerTmuxPane onSendReady).
    const response = await postRunInput(runId, text);
    if (!response.accepted) {
      throw new Error(response.message ?? 'Agent rejected input.');
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
        {readOnly ? (
          <ReadOnlySessionBar />
        ) : (
          <ManagerComposer
            runId={runId}
            connectionState="open"
            initialDraft={initialDraft}
            placeholder="Send to agent"
            readyLabel="Enter sends. Shift+Enter adds a line. Arrow-up recalls the last send."
            onSend={handleSend}
            onSent={handleSent}
          />
        )}
      </div>
    </div>
  );
}

function RunTmuxStack({
  runId,
  initialDraft,
  onPromptSent,
  readOnly,
}: {
  runId: string;
  initialDraft?: string | null;
  onPromptSent: () => void;
  readOnly: boolean;
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
            readOnly={readOnly}
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
        {readOnly ? (
          <ReadOnlySessionBar />
        ) : (
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
        )}
      </div>
    </div>
  );
}
