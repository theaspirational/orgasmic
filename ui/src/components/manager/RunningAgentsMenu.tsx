import { useCallback } from 'react';
import { Radio } from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { useEventStream } from '@/hooks/useEventStream';
import { fetchRuns } from '@/lib/api';
import { MANAGER_TAB_ID, useRunDock } from '@/lib/runDock';
import { isManagerRun, runTabTitle } from '@/lib/runLabels';
import type { DaemonEvent, RecoveredRun, RunSummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

function recoveredTitle(run: RecoveredRun): string {
  // Recovered runs do not carry a task id in the recovery payload, so fall back
  // to the session path tail for a readable label.
  const tail = run.session_path.split('/').pop() ?? run.run_id;
  return tail.replace(/\.jsonl$/i, '');
}

export function RunningAgentsMenu({ projectId }: { projectId: string | null }) {
  const { openRun, activeTabId, size } = useRunDock();
  const runs = useResource('rundock-running-agents', fetchRuns);

  const refresh = useCallback(() => {
    void runs.refresh();
  }, [runs]);

  useEventStream(
    useCallback(
      (event: DaemonEvent) => {
        if (event.topic === 'run' && event.payload.kind !== 'run_event') refresh();
        if (event.topic === 'manager') refresh();
      },
      [refresh],
    ),
  );

  const live = runs.data?.live ?? [];
  // Workers and managers are both "running"; the dock surfaces the manager via
  // its special tab, but it stays selectable here for symmetry.
  const running = live;
  // Recent defaults to the current project: terminal/no-op runs from this boot
  // plus any ambiguous ones. Global toggle/search is deferred.
  const recent: RecoveredRun[] = [
    ...(runs.data?.terminal_noop ?? []),
    ...(runs.data?.ambiguous ?? []),
  ].filter((run) => !projectId || !run.session_path || run.session_path.includes(projectId));

  const count = running.length;

  const handleSelectRunning = (run: RunSummary) => {
    const role = isManagerRun(run) ? 'manager' : 'worker';
    const tabId = role === 'manager' ? MANAGER_TAB_ID : run.run_id;
    // Per dec_053: first attach opens peek. Re-selecting the already-active tab
    // should focus it without collapsing workbench/focus.
    openRun({
      runId: run.run_id,
      role,
      size: activeTabId === tabId ? size : 'peek',
    });
  };

  return (
    <DropdownMenu onOpenChange={(next) => next && refresh()}>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="gap-1.5"
          aria-label={`Running agents${count ? `: ${count}` : ''}`}
        >
          <Radio className="size-4" />
          <span className="hidden sm:inline">Running Agents</span>
          {count > 0 ? (
            <span className="rounded-full border px-1.5 font-mono text-[10px] text-muted-foreground">
              {count}
            </span>
          ) : null}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-80">
        <DropdownMenuLabel className="text-[11px] uppercase tracking-wide text-muted-foreground">
          Running
        </DropdownMenuLabel>
        {running.length === 0 ? (
          <DropdownMenuItem disabled>No running agents</DropdownMenuItem>
        ) : (
          running.map((run) => (
            <DropdownMenuItem
              key={run.run_id}
              onClick={() => handleSelectRunning(run)}
              title={run.run_id}
            >
              <span className="min-w-0 flex-1 truncate">{runTabTitle(run)}</span>
              {run.sub_state ? (
                <span className="font-mono text-[10px] text-muted-foreground">
                  {run.sub_state}
                </span>
              ) : null}
              <span className="font-mono text-[10px] text-muted-foreground">
                {run.event_count}
              </span>
            </DropdownMenuItem>
          ))
        )}
        <DropdownMenuSeparator />
        <DropdownMenuLabel className="text-[11px] uppercase tracking-wide text-muted-foreground">
          Recent
        </DropdownMenuLabel>
        {recent.length === 0 ? (
          <DropdownMenuItem disabled>No recent runs</DropdownMenuItem>
        ) : (
          recent.slice(0, 6).map((run) => (
            <DropdownMenuItem key={run.run_id} disabled title={run.run_id}>
              <span className="min-w-0 flex-1 truncate text-muted-foreground">
                {recoveredTitle(run)}
              </span>
              <span className="font-mono text-[10px] text-muted-foreground">{run.classification}</span>
            </DropdownMenuItem>
          ))
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
