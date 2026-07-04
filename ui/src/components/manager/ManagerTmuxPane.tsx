// @arch arch_MK2Q2.5
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { FitAddon } from '@xterm/addon-fit';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { Terminal } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';

import { transport } from '@/lib/transport';
import { cn } from '@/lib/utils';

export type TmuxPaneConnectionState = 'connecting' | 'open' | 'closed' | 'error';
export type TmuxSendKeys = (text: string) => boolean;

const MAX_RECONNECT_ATTEMPTS = 5;
const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 10_000;
/** Trailing debounce for ResizeObserver ticks so window drags don't flood the PTY. */
const RESIZE_DEBOUNCE_MS = 100;

type TmuxPaneServerFrame =
  | { type: 'pane_delta'; text: string }
  | { type: 'pane_full'; text: string }
  | { type: 'error'; message?: string };

function parseTmuxPaneServerFrame(data: string): TmuxPaneServerFrame | null {
  try {
    const parsed = JSON.parse(data) as { type?: unknown; text?: unknown; message?: unknown };
    if (parsed.type === 'pane_delta' && typeof parsed.text === 'string') {
      return { type: 'pane_delta', text: parsed.text };
    }
    if (parsed.type === 'pane_full' && typeof parsed.text === 'string') {
      return { type: 'pane_full', text: parsed.text };
    }
    if (parsed.type === 'error') {
      return { type: 'error', message: typeof parsed.message === 'string' ? parsed.message : undefined };
    }
  } catch {
    return null;
  }
  return null;
}

/**
 * In-browser xterm.js terminal bridged to the manager run's tmux session over
 * the daemon's PTY-attach WebSocket (`/ws/tmux/:run_id`). The pane renders the
 * raw byte stream live; typing, arrow keys, and ctrl-combos flow through the
 * PTY, and resizes reshape it via TIOCSWINSZ (transport lifted from HAR).
 *
 * - Debounced resize: ResizeObserver ticks coalesce behind a 100ms trailing
 *   debounce before fitting and sending the `resize` control frame.
 * - Auto-reconnect: non-user-initiated WS closes retry with exponential
 *   backoff (1s → 2s → 4s → max 10s) up to 5 attempts.
 * - Composer seam: `onSendReady` hands the parent a sender that pastes text
 *   and presses Enter server-side (`send_keys`), independent of the keyboard.
 * - Read-only mode (`readOnly`): for members without sessions.interact. The
 *   live stream still renders, but nothing is wired to the socket — no
 *   keystrokes, Shift+Enter, wheel-as-keys, or resize control frames — and the
 *   composer sender is never handed out. A "read-only" chip marks the header.
 */
export function ManagerTmuxPane({
  runId,
  onConnectionState,
  onSendReady,
  readOnly = false,
}: {
  runId: string;
  onConnectionState?: (state: TmuxPaneConnectionState) => void;
  onSendReady?: (send: TmuxSendKeys | null) => void;
  readOnly?: boolean;
}) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const [connState, setConnState] = useState<TmuxPaneConnectionState>('connecting');
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  /** True when the component is unmounting — suppresses auto-reconnect. */
  const userDetachedRef = useRef(false);
  const resizeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** Disposable for xterm onData — replaced on each connectWs call. */
  const onDataRef = useRef<{ dispose: () => void } | null>(null);

  // Stable wire encoder so we don't churn one per keystroke.
  const codec = useMemo(() => ({ enc: new TextEncoder() }), []);

  useEffect(() => {
    onConnectionState?.(connState);
  }, [connState, onConnectionState]);

  const sendResize = useCallback(() => {
    const term = termRef.current;
    const fit = fitRef.current;
    const ws = wsRef.current;
    if (!term || !fit) return;
    try {
      fit.fit();
    } catch {
      return; /* xterm can reject fit while hidden during layout transitions. */
    }
    // Read-only viewers fit locally for display but never reshape the shared PTY.
    if (!readOnly && ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: 'resize', cols: term.cols, rows: term.rows }));
    }
  }, [readOnly]);

  const debouncedResize = useCallback(() => {
    if (resizeTimerRef.current !== null) clearTimeout(resizeTimerRef.current);
    resizeTimerRef.current = setTimeout(() => {
      resizeTimerRef.current = null;
      sendResize();
    }, RESIZE_DEBOUNCE_MS);
  }, [sendResize]);

  /** Open (or re-open) the WS connection, wiring it to the existing Terminal. */
  const connectWs = useCallback(
    (term: Terminal) => {
      onDataRef.current?.dispose();
      onDataRef.current = null;

      const ws = transport.openWebSocket(`/ws/tmux/${encodeURIComponent(runId)}`);
      ws.binaryType = 'arraybuffer';
      wsRef.current = ws;

      ws.onopen = () => {
        if (userDetachedRef.current) return;
        setConnState('open');
        setAttempt(0);
        setError(null);
        if (readOnly) {
          // No input channel for a read-only viewer: never reshape the PTY and
          // never hand the parent a sender.
          onSendReady?.(null);
          return;
        }
        // Reshape the PTY to our real dimensions before tmux's first frame.
        try {
          ws.send(JSON.stringify({ type: 'resize', cols: term.cols, rows: term.rows }));
        } catch {
          /* socket may already be closing */
        }
        onSendReady?.((text: string) => {
          if (ws.readyState !== WebSocket.OPEN) return false;
          ws.send(JSON.stringify({ type: 'send_keys', text }));
          return true;
        });
      };

      ws.onmessage = (event) => {
        if (event.data instanceof ArrayBuffer) {
          term.write(new Uint8Array(event.data));
        } else if (typeof event.data === 'string') {
          // Control / error frame, or the mock bridge's JSON pane protocol.
          const frame = parseTmuxPaneServerFrame(event.data);
          if (frame?.type === 'error') {
            if (frame.message) setError(frame.message);
            return;
          }
          if (frame?.type === 'pane_full') {
            term.clear();
            term.write(frame.text);
            return;
          }
          if (frame?.type === 'pane_delta') {
            term.write(frame.text);
          }
        }
      };

      ws.onerror = () => {
        if (userDetachedRef.current) return;
        setConnState('error');
      };

      ws.onclose = () => {
        wsRef.current = null;
        onSendReady?.(null);
        if (userDetachedRef.current) {
          setConnState('closed');
          return;
        }
        // Auto-reconnect with exponential backoff.
        setAttempt((prev) => {
          const next = prev + 1;
          if (next > MAX_RECONNECT_ATTEMPTS) {
            setConnState('closed');
            return next;
          }
          setConnState('connecting');
          const delay = Math.min(RECONNECT_BASE_MS * 2 ** (next - 1), RECONNECT_MAX_MS);
          reconnectTimerRef.current = setTimeout(() => {
            reconnectTimerRef.current = null;
            if (termRef.current) connectWs(termRef.current);
          }, delay);
          return next;
        });
      };

      // Read-only viewers never forward keystrokes/paste to the PTY.
      if (!readOnly) {
        const disposable = term.onData((data) => {
          if (ws.readyState !== WebSocket.OPEN) return;
          ws.send(codec.enc.encode(data));
        });
        onDataRef.current = disposable;
      }
    },
    [runId, codec.enc, onSendReady, readOnly],
  );

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return undefined;

    userDetachedRef.current = false;
    setAttempt(0);
    setError(null);
    setConnState('connecting');
    onSendReady?.(null);

    const term = new Terminal({
      cursorBlink: true,
      convertEol: false,
      fontFamily:
        'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", monospace',
      fontSize: 13,
      // Match rmux's web-share frontend: 1.2 line-height gives TUI output a bit
      // of breathing room and reduces glyph-row overlap.
      lineHeight: 1.2,
      scrollback: 5000,
      // Borrowed from rmux's web-share terminal: on macOS, treat Option as Meta
      // so the wrapped TUIs receive Alt/Meta chords (Alt+arrows, Meta+Enter)
      // instead of composed accented characters.
      macOptionIsMeta: true,
      theme: {
        background: '#050505',
        foreground: '#e5e7eb',
      },
      // Required so xterm v6's still-experimental Unicode API is callable.
      allowProposedApi: true,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    // Clickable URLs in agent output — open in a new tab (rmux's frontend wires
    // the same WebLinksAddon).
    term.loadAddon(
      new WebLinksAddon((event, uri) => {
        if (event.button === 0) window.open(uri, '_blank', 'noopener,noreferrer');
      }),
    );
    term.open(host);
    // Defer fit until after the host has its final layout size.
    const raf = window.requestAnimationFrame(() => {
      try {
        fit.fit();
      } catch {
        /* host not yet sized */
      }
    });
    termRef.current = term;
    fitRef.current = fit;

    // xterm.js emits a plain `\r` for both Enter and Shift+Enter (the legacy
    // VT protocol has no Shift+Enter encoding). Claude Code, Codex CLI, and
    // most modern TUI agents read `\x1b\r` (Esc+CR, the Alt/Meta+Enter form)
    // as "insert newline", so map Shift+Enter to that. preventDefault keeps
    // xterm's hidden textarea from replaying a stray `\n`.
    const onShiftEnter = (e: KeyboardEvent) => {
      if (readOnly) return true;
      if (
        e.type === 'keydown' &&
        e.key === 'Enter' &&
        e.shiftKey &&
        !e.ctrlKey &&
        !e.metaKey &&
        !e.altKey
      ) {
        e.preventDefault();
        e.stopPropagation();
        const ws = wsRef.current;
        if (ws && ws.readyState === WebSocket.OPEN) {
          ws.send(codec.enc.encode('\x1b\r'));
        }
        return false;
      }
      return true;
    };
    term.attachCustomKeyEventHandler(onShiftEnter);

    // Mouse-wheel scrolling. The wrapped rmux TUI (Claude Code, Codex CLI) runs
    // on the alternate screen buffer where xterm has no scrollback to move, so a
    // bare wheel event is silently dropped and scrolling appears dead. Translate
    // it into cursor up/down keys the TUI understands — the same convention
    // terminal emulators use for "alternate scroll" mode. On the normal buffer we
    // return true so xterm's native scrollback handling stays intact.
    term.attachCustomWheelEventHandler((e: WheelEvent) => {
      // Read-only viewers don't drive alternate-scroll into the PTY.
      if (readOnly) return true;
      if (term.buffer.active.type !== 'alternate') return true;
      const ws = wsRef.current;
      if (!ws || ws.readyState !== WebSocket.OPEN) return false;
      e.preventDefault();
      // deltaMode: 1 = lines, 0 = pixels (~24px per row), 2 = pages.
      const rows = e.deltaMode === 1 ? Math.abs(e.deltaY) : Math.abs(e.deltaY) / 24;
      const count = Math.min(10, Math.max(1, Math.round(rows)));
      const seq = e.deltaY > 0 ? '\x1b[B' : '\x1b[A';
      ws.send(codec.enc.encode(seq.repeat(count)));
      return false;
    });

    // Belt-and-suspenders: catch Shift+Enter before xterm's textarea sees it.
    const captureHandler = (ev: KeyboardEvent) => {
      onShiftEnter(ev);
    };
    host.addEventListener('keydown', captureHandler, { capture: true });

    connectWs(term);

    const resizeObserver = new ResizeObserver(() => debouncedResize());
    resizeObserver.observe(host);
    window.addEventListener('resize', debouncedResize);

    return () => {
      userDetachedRef.current = true;
      onSendReady?.(null);
      host.removeEventListener('keydown', captureHandler, { capture: true });
      onDataRef.current?.dispose();
      onDataRef.current = null;
      window.cancelAnimationFrame(raf);
      window.removeEventListener('resize', debouncedResize);
      resizeObserver.disconnect();
      if (resizeTimerRef.current !== null) {
        clearTimeout(resizeTimerRef.current);
        resizeTimerRef.current = null;
      }
      if (reconnectTimerRef.current !== null) {
        clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
      // Best-effort graceful detach so tmux sees a clean client disconnect.
      const ws = wsRef.current;
      try {
        if (ws && ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: 'detach' }));
        }
      } catch {
        /* ignore */
      }
      ws?.close();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
      wsRef.current = null;
    };
  }, [runId, connectWs, debouncedResize, codec.enc, onSendReady]);

  return (
    <div className="relative flex h-full min-h-0 flex-col bg-black">
      <div className="flex shrink-0 items-center gap-2 border-b border-white/10 px-3 py-1.5 font-mono text-xs text-white/60">
        <span className="truncate">{runId}</span>
        {readOnly ? (
          <span className="rounded-sm border border-white/20 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-white/70">
            read-only
          </span>
        ) : null}
        {/* Connection STATUS, not an action — a bare uppercase "OPEN" at the
            edge of the banner reads as a clickable link. Pair it with a state
            dot so it scans as telemetry. */}
        <span className="ml-auto flex items-center gap-1.5 uppercase">
          <span
            aria-hidden
            className={cn(
              'size-1.5 rounded-full',
              connState === 'open' && 'bg-emerald-400',
              connState === 'connecting' && 'bg-amber-400',
              (connState === 'closed' || connState === 'error') && 'bg-red-400',
            )}
          />
          {connState === 'connecting' && attempt > 0
            ? `reconnecting ${attempt}/${MAX_RECONNECT_ATTEMPTS}`
            : connState}
        </span>
      </div>
      {error ? (
        <div
          className="shrink-0 border-b border-destructive/40 bg-destructive/15 px-3 py-2 text-xs leading-snug text-destructive"
          role="alert"
          aria-live="polite"
        >
          <p className="break-words">{error}</p>
        </div>
      ) : null}
      <div
        ref={hostRef}
        className="min-h-0 flex-1 overflow-hidden p-2"
        onMouseDownCapture={() => {
          termRef.current?.focus();
        }}
        onPointerDownCapture={() => {
          termRef.current?.focus();
        }}
      />
      {connState === 'connecting' && attempt === 0 ? (
        <div className="absolute inset-0 flex items-center justify-center bg-black/55 text-sm text-white/70">
          Connecting terminal...
        </div>
      ) : null}
    </div>
  );
}
