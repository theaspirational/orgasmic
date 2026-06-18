import { useCallback } from 'react';

import { useEventStream } from '@/hooks/useEventStream';
import { fetchRuns } from '@/lib/api';
import type { DaemonEvent, RunSummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

export type TaskRunMatch = {
  running: RunSummary[];
};

export function useTaskRuns(): {
  loading: boolean;
  forTask: (taskId: string) => TaskRunMatch;
} {
  const runs = useResource('task-badges-runs', fetchRuns);

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
