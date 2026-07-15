import { useCallback } from 'react';
import { Eye, Radio } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import type { TaskRunMatch } from '@/hooks/useTaskRuns';
import { useRunDock } from '@/lib/runDock';
import { runTabTitle } from '@/lib/runLabels';
import { buildPerformerPill } from '@/lib/performerPill';
import type { RunSummary } from '@/lib/types';
import { cn } from '@/lib/utils';

// `run.role` is who is working right now (the resolved worker's kind).
// Pre-role daemons only kept the dispatch role in the session filename
// (`dispatch-<TASK>-<role>-<stamp>.jsonl`); fall back to that, then to the
// bare run surface.
function runRole(run: RunSummary): string {
  if (run.role) return run.role;
  const match = /(?:^|\/)dispatch-.+-(implementer|reviewer|architector)-\d{8}T\d{6}\.jsonl$/.exec(
    run.session_path ?? '',
  );
  return match?.[1] ?? run.kind;
}

// Performer pill label (dec_041 dot separator, dec_068 sub-state vocab).
// PTY-driven dispatch runs have no self-reporting channel yet; sub_state falls
// back to "<role>.working" so a live run is its performer at work by definition.
function liveBadgeLabel(run: RunSummary): string {
  const role = runRole(run);
  const subState = run.sub_state?.trim() || `${role}.working`;
  const pill = buildPerformerPill(role, run.worker_id, subState, true);
  return pill?.label ?? 'live';
}

// Running agent badges for a task. Historical recoverable sessions are a
// manager/session concern, not task-local state; showing them here makes old
// filename-derived matches look like live work on the task.
export function TaskAgentBadges({
  match,
  className,
  onOpen,
}: {
  match: TaskRunMatch;
  className?: string;
  // Fired after a run is opened in the dock (e.g. to dismiss a containing modal
  // so the dock surface is visible).
  onOpen?: () => void;
}) {
  const { openRun: openRunRaw } = useRunDock();
  const { running } = match;

  const openRun = useCallback(
    (options: Parameters<typeof openRunRaw>[0]) => {
      openRunRaw(options);
      onOpen?.();
    },
    [onOpen, openRunRaw],
  );

  if (running.length === 0) return null;

  // Babysitters watch a performer; they never headline the badge. One click
  // goes straight to the performer's session — babysitter-only tasks still
  // badge the babysitter, and the grouped menu always lists every run.
  const performers = running.filter((run) => run.kind !== 'babysitter');
  const primary = performers.length > 0 ? performers : running;

  // When a performer headlines the badge, surface its companion babysitter as a
  // small watcher dot stacked on the performer badge — one click opens the
  // babysitter's live transcript, mirroring the performer click. A
  // babysitter-only task badges the babysitter as primary, so no overlay there.
  const watcher = performers.length > 0 ? running.find((run) => run.kind === 'babysitter') : undefined;

  const total = running.length;
  const singleImmediate = primary.length === 1;

  function handleClick(event: React.MouseEvent) {
    // Badges live inside clickable task rows; never bubble to the row's open.
    event.preventDefault();
    event.stopPropagation();
    if (!singleImmediate) return;
    const run = primary[0]!;
    openRun({ runId: run.run_id });
  }

  const liveBadge = (
    <Badge
      variant="default"
      className="gap-1 font-mono text-[10px]"
      title={running.map((r) => r.run_id).join(', ')}
    >
      <Radio className="size-2.5" />
      {singleImmediate ? liveBadgeLabel(primary[0]!) : `${total} live`}
    </Badge>
  );

  // Watcher dot — overlaps the performer badge's top-right corner. The ring in
  // the page background colour lifts it off the badge so it reads as a distinct
  // second agent rather than part of the performer pill.
  const watcherDot = watcher ? (
    <button
      type="button"
      onClick={(event) => {
        event.preventDefault();
        event.stopPropagation();
        openRun({ runId: watcher.run_id });
      }}
      title={`${liveBadgeLabel(watcher)} · open transcript`}
      aria-label="Open babysitter run"
      className="absolute -right-1.5 -top-1.5 z-10 rounded-full outline-none transition focus-visible:ring-2 focus-visible:ring-ring/50"
    >
      <span className="flex size-3.5 items-center justify-center rounded-full bg-amber-400 text-black shadow-sm ring-2 ring-background hover:bg-amber-300 dark:bg-amber-500 dark:hover:bg-amber-400">
        <Eye className="size-2" />
      </span>
    </button>
  ) : null;

  const badges = (
    <div className={cn('flex flex-wrap items-center gap-1', className)}>
      {running.length > 0 ? liveBadge : null}
    </div>
  );

  if (singleImmediate) {
    return (
      <div className={cn('relative inline-flex', className)}>
        <button
          type="button"
          onClick={handleClick}
          className="rounded-md outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
          aria-label="Open agent run"
        >
          {liveBadge}
        </button>
        {watcherDot}
      </div>
    );
  }

  // Multiple relevant runs: a grouped menu.
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          onClick={(event) => {
            event.preventDefault();
            event.stopPropagation();
          }}
          className="rounded-md outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
          aria-label={`Agent runs: ${total}`}
        >
          {badges}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="end"
        className="w-72"
        onClick={(event) => event.stopPropagation()}
      >
        <DropdownMenuLabel className="text-[11px] uppercase tracking-wide text-muted-foreground">
          Running
        </DropdownMenuLabel>
        {running.map((run) => (
          <DropdownMenuItem
            key={run.run_id}
            title={run.run_id}
            onClick={() =>
              openRun({ runId: run.run_id })
            }
          >
            <span className="min-w-0 flex-1 truncate">{runTabTitle(run)}</span>
            <span className="font-mono text-[10px] text-muted-foreground">
              {run.event_count}
            </span>
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
