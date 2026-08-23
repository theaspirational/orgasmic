// Daemon events have no replay: whatever fired while the WebSocket was down is
// gone, so anything derived from them is stale until it refetches. The stream
// (hooks/useEventStream) raises this on reconnect; useResource listens.
const listeners = new Set<() => void>();

/** Subscribe to "you missed events, refetch"; returns an unsubscribe fn. */
export function subscribeResync(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function notifyResync(): void {
  for (const listener of Array.from(listeners)) listener();
}
