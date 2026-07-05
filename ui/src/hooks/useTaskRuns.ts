import { useCallback } from 'react';

import { useEventStream } from '@/hooks/useEventStream';
import { useMe } from '@/hooks/useMe';
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
  // Run badges read the admin-only `/runs` list; members 403 there, so skip the
  // poll (they see tasks without live-run badges) rather than error on it.
  const { isMember } = useMe();
  const runs = useResource('task-badges-runs', fetchRuns, { enabled: !isMember });

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
