import type { ManagerDriverProfile } from '@/lib/types';

// Bare terminal sessions attach through the daemon PTY bridge; rmux survives a
// daemon restart, so it wins over tmux when both are provisioned.
const TERMINAL_MODE_PREFERENCE = ['rmux', 'tmux'];

const SYSTEM_WIDE_RMUX_KEY = 'orgasmic.manager.rmuxSystemWide';

/** Whether rmux sessions launch system-wide (detached from the daemon,
 * surviving restarts). Defaults ON. */
export function readRmuxSystemWide(): boolean {
  if (typeof window === 'undefined') return true;
  return window.localStorage.getItem(SYSTEM_WIDE_RMUX_KEY) !== 'false';
}

/** `system_wide` value for a launch with this driver. Only rmux sessions can
 * detach from the daemon; other modes always send false. */
export function launchSystemWide(driver: { mode: string }): boolean {
  return driver.mode === 'rmux' && readRmuxSystemWide();
}

/** The driver a taskbar Terminal launch should use: the `custom` pseudo-harness
 * (no agent CLI) on the best available PTY mode. */
export function resolveTerminalDriver(
  installed: ManagerDriverProfile[],
): ManagerDriverProfile | null {
  const candidates = installed.filter(
    (driver) =>
      driver.harness === 'custom' && driver.installed && driver.mode_installed !== false,
  );
  for (const mode of TERMINAL_MODE_PREFERENCE) {
    const preferred = candidates.find((driver) => driver.mode === mode);
    if (preferred) return preferred;
  }
  return candidates[0] ?? null;
}
