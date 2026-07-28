import { useCallback } from 'react';

import { useEventStream } from '@/hooks/useEventStream';
import { useMe } from '@/hooks/useMe';
import { fetchLiveRuns } from '@/lib/api';
import type { DaemonEvent, RunSummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

export type TaskRunMatch = {
  running: RunSummary[];
};

export function useTaskRuns(): {
  loading: boolean;
  forTask: (taskId: string) => TaskRunMatch;
} {
  // orgasmic:task_6HJYT — a task badge asks "is this task running right now",
  // which is live-state-per-task, not durable history. It reads the
  // supervisor-local `/runs/live`, so the whole-board recovery scan is not on
  // the path of a badge that repolls on every run event.
  //
  // Still admin-only; members 403 there, so skip the poll (they see tasks
  // without live-run badges) rather than error on it.
  const { isMember } = useMe();
  const runs = useResource('task-badges-runs', fetchLiveRuns, { enabled: !isMember });

  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        if (event.topic === 'run' && event.payload.kind !== 'run_event') {
          void runs.refresh();
        }
      },
      [runs],
    ),
  );

  const live = runs.data?.live ?? [];

  const forTask = useCallback(
    (taskId: string): TaskRunMatch => {
      const running = live.filter((run) => run.task_id === taskId);
      return { running };
    },
    [live],
  );

  return {
    loading: runs.loading && !runs.data,
    forTask,
  };
}
