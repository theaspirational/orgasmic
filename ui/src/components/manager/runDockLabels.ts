import { isManagerRun, runTabTitle } from '@/lib/runLabels';
import type { RunSummary } from '@/lib/types';

export function workerRunTabLabel(
  runId: string | null,
  liveRun: RunSummary | null | undefined,
  labelCache: Record<string, string>,
): string {
  if (liveRun) return runTabTitle(liveRun);
  if (runId && labelCache[runId]) return labelCache[runId];
  return runId ?? 'Run';
}

// A bare terminal session launched from the taskbar's Terminal shortcut: it
// rides the manager.launch task namespace but carries the `custom`
// pseudo-harness (no agent CLI), so it must never claim the Manager button.
export function isTerminalRun(run: Pick<RunSummary, 'task_id' | 'harness'>): boolean {
  return isManagerRun(run) && (run.harness ?? '').trim().toLowerCase() === 'custom';
}

export function terminalRunLabel(index: number, total: number): string {
  return total > 1 ? `Terminal ${index + 1}` : 'Terminal';
}

// A manager session started outside the app (dec_3Y2E1): a real supervised
// run with no PTY behind it. It renders as an info row — there is nothing to
// attach — with an inline End control instead of the usual open-tab click.
export function isExternalManagerRun(run: Pick<RunSummary, 'driver'>): boolean {
  return (run.driver ?? '').trim().toLowerCase() === 'external';
}

// "Running Agents" answers "which agents is orgasmic supervising?". A bare
// terminal is a PTY the operator drives, not an agent, so it lives on the
// taskbar only and never counts toward the badge.
export function agentRuns<T extends Pick<RunSummary, 'task_id' | 'harness'>>(runs: T[]): T[] {
  return runs.filter((run) => !isTerminalRun(run));
}

// The runs endpoint makes no ordering promise, but taskbar labels are positional
// ("Terminal 2") and buttons must not swap places between refreshes. A run id
// leads with its launch stamp (run-<YYYYMMDDTHHMMSS>-<uuid>), so ordering by it
// is oldest-first and stays stable when two runs land in the same second.
export function orderRunsByLaunch<T extends Pick<RunSummary, 'run_id'>>(runs: T[]): T[] {
  return [...runs].sort((a, b) => a.run_id.localeCompare(b.run_id));
}

// Taskbar buttons carry the short task id; the full provider-qualified title
// (runTabTitle) stays in the tooltip.
export function workerButtonLabel(run: Pick<RunSummary, 'task_id'>): string {
  return run.task_id.trim() || 'Run';
}
