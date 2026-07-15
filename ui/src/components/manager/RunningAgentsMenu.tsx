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
import { useRunDock } from '@/lib/runDock';
import { runTabTitle } from '@/lib/runLabels';
import type { DaemonEvent, RecoveredRun, RunSummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { agentRuns } from './runDockLabels';

function recoveredTitle(run: RecoveredRun): string {
  // Recovered runs do not carry a task id in the recovery payload, so fall back
  // to the session path tail for a readable label.
  const tail = run.session_path.split('/').pop() ?? run.run_id;
  return tail.replace(/\.jsonl$/i, '');
}

export function RunningAgentsMenu({ projectId }: { projectId: string | null }) {
  const { openRun } = useRunDock();
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

  // Terminals are peer runs on the taskbar but they are not agents: this menu
  // (and its count badge) reports what orgasmic is supervising, so only worker
  // and agent-manager runs make the list.
  const running = agentRuns(runs.data?.live ?? []);
  // Recent defaults to the current project: terminal/no-op runs from this boot
  // plus any ambiguous ones. Global toggle/search is deferred.
  const recent: RecoveredRun[] = [
    ...(runs.data?.terminal_noop ?? []),
    ...(runs.data?.ambiguous ?? []),
  ].filter((run) => !projectId || !run.session_path || run.session_path.includes(projectId));

  const count = running.length;

  // Attaching from the menu raises the run exactly like clicking its taskbar
  // button: same tab, same remembered height.
  const handleSelectRunning = (run: RunSummary) => {
    openRun({ runId: run.run_id });
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
