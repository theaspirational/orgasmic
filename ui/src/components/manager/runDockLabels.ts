import { runTabTitle } from '@/lib/runLabels';
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
