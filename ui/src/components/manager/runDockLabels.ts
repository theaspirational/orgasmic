import {
  isExternalManagerRun,
  isManagerRun,
  isRunDockEligible,
  runTabTitle,
} from '@/lib/runLabels';
import { compareRunIdsByLaunch } from '@/lib/runId';
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
export function isTerminalRun(
  run: Pick<RunSummary, 'task_id' | 'harness' | 'claimed_manager'>,
): boolean {
  return (
    isManagerRun(run) &&
    !run.claimed_manager &&
    (run.harness ?? '').trim().toLowerCase() === 'custom'
  );
}

export function terminalRunLabel(index: number, total: number): string {
  return total > 1 ? `Terminal ${index + 1}` : 'Terminal';
}

// A manager session started outside the app (dec_3Y2E1): a real supervised
// run with no PTY behind it. It renders as an info row — there is nothing to
// attach — with an inline End control instead of the usual open-tab click.
export { isExternalManagerRun };

export function taskbarRunGroups(runs: RunSummary[]): {
  managers: RunSummary[];
  terminals: RunSummary[];
  workers: RunSummary[];
} {
  const eligible = orderRunsByLaunch(runs.filter(isRunDockEligible));
  return {
    managers: eligible.filter((run) => isManagerRun(run) && !isTerminalRun(run)),
    terminals: eligible.filter(isTerminalRun),
    workers: eligible.filter((run) => !isManagerRun(run)),
  };
}

// "Running Agents" answers "which agents is orgasmic supervising?". A bare
// terminal is a PTY the operator drives, not an agent, so it lives on the
// taskbar only and never counts toward the badge.
export function agentRuns<T extends Pick<RunSummary, 'task_id' | 'harness' | 'claimed_manager'>>(
  runs: T[],
): T[] {
  return runs.filter((run) => !isTerminalRun(run));
}

// The runs endpoint makes no ordering promise, but taskbar labels are positional
// ("Terminal 2") and buttons must not swap places between refreshes. Decode the
// launch time from both compact ULIDs and historical timestamp+UUID ids; the id
// itself remains the deterministic tie-breaker for same-millisecond launches.
export function orderRunsByLaunch<T extends Pick<RunSummary, 'run_id'>>(runs: T[]): T[] {
  return [...runs].sort((a, b) => compareRunIdsByLaunch(a.run_id, b.run_id));
}

// Taskbar buttons carry the short task id; the full provider-qualified title
// (runTabTitle) stays in the tooltip.
export function workerButtonLabel(run: Pick<RunSummary, 'task_id'>): string {
  return run.task_id.trim() || 'Run';
}
