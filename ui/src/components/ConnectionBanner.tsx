// @arch arch_MK2Q2.4
import type { WsConnectionState } from '../lib/types';

const LABELS: Record<WsConnectionState, string> = {
  connecting: 'WS connecting',
  open: 'WS live',
  reconnecting: 'WS reconnecting',
  closed: 'WS closed',
};

export function WsIndicator({ state }: { state: WsConnectionState }) {
  return (
    <span className={`ws-dot ws-${state}`} title={LABELS[state]}>
      {LABELS[state]}
    </span>
  );
}

export function ConnectionBanner({ wsState }: { wsState: WsConnectionState }) {
  if (wsState === 'reconnecting') {
    return (
      <div className="banner banner-warn" role="status">
        Reconnecting to daemon event stream… classifications will refresh when connected.
      </div>
    );
  }
  if (wsState === 'connecting') {
    return (
      <div className="banner banner-neutral" role="status">
        Connecting to daemon event stream…
      </div>
    );
  }
  return null;
}
