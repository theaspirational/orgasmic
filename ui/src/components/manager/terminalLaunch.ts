import type { ManagerDriverProfile } from '@/lib/types';

/** The driver a taskbar Terminal launch should use: the `custom` pseudo-harness
 * (no agent CLI) on tmux. */
export function resolveTerminalDriver(
  installed: ManagerDriverProfile[],
): ManagerDriverProfile | null {
  const candidates = installed.filter(
    (driver) =>
      driver.mode === 'tmux' && driver.harness === 'custom' && driver.installed,
  );
  return candidates[0] ?? null;
}
