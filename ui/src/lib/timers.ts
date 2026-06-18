export type CancelTimer = () => void;

export function scheduleOnce(ms: number, fn: () => void): CancelTimer {
  const id = window.setTimeout(fn, ms);
  return () => window.clearTimeout(id);
}
