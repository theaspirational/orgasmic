// @arch arch_MK2Q2.3
import { useEffect, useRef, useState } from 'react';

import { transport } from '../lib/transport';
import type { DaemonEvent, WsConnectionState } from '../lib/types';

const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 15_000;

type EventCallback = (event: DaemonEvent) => void;
type StatusCallback = (state: WsConnectionState) => void;

const listeners = new Set<EventCallback>();
const statusListeners = new Set<StatusCallback>();

let socket: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let reconnectAttempt = 0;
let connectionState: WsConnectionState = 'closed';

function setConnectionState(next: WsConnectionState): void {
  if (connectionState === next) return;
  connectionState = next;
  for (const listener of Array.from(statusListeners)) listener(next);
}

function dispatch(event: DaemonEvent): void {
  for (const listener of Array.from(listeners)) listener(event);
}

function clearReconnectTimer(): void {
  if (reconnectTimer !== null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
}

function disconnectIfIdle(): void {
  if (listeners.size > 0 || statusListeners.size > 0) return;
  clearReconnectTimer();
  reconnectAttempt = 0;
  if (socket) {
    const closing = socket;
    socket = null;
    closing.close();
  }
  setConnectionState('closed');
}

function connect(): void {
  if ((listeners.size === 0 && statusListeners.size === 0) || socket !== null || reconnectTimer !== null) {
    return;
  }

  setConnectionState(reconnectAttempt > 0 ? 'reconnecting' : 'connecting');
  socket = transport.openWebSocket('/ws');

  socket.onopen = () => {
    reconnectAttempt = 0;
    setConnectionState('open');
  };

  socket.onmessage = (event) => {
    try {
      const parsed = JSON.parse(String(event.data)) as DaemonEvent;
      if (parsed && typeof parsed.topic === 'string') dispatch(parsed);
    } catch {
      /* ignore */
    }
  };

  socket.onerror = () => {
    /* onclose handles reconnect */
  };

  socket.onclose = () => {
    socket = null;
    if (listeners.size === 0 && statusListeners.size === 0) {
      setConnectionState('closed');
      return;
    }
    reconnectAttempt += 1;
    setConnectionState('reconnecting');
    const delay = Math.min(RECONNECT_BASE_MS * 2 ** (reconnectAttempt - 1), RECONNECT_MAX_MS);
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, delay);
  };
}

export function useEventStream(onEvent: EventCallback): void {
  const callbackRef = useRef(onEvent);
  callbackRef.current = onEvent;

  useEffect(() => {
    const listener: EventCallback = (event) => callbackRef.current(event);
    listeners.add(listener);
    connect();
    return () => {
      listeners.delete(listener);
      disconnectIfIdle();
    };
  }, []);
}

export function useWsStatus(): WsConnectionState {
  const [state, setState] = useState<WsConnectionState>(() => connectionState);

  useEffect(() => {
    const listener: StatusCallback = (next) => setState(next);
    statusListeners.add(listener);
    connect();
    setState(connectionState);
    return () => {
      statusListeners.delete(listener);
      disconnectIfIdle();
    };
  }, []);

  return state;
}
