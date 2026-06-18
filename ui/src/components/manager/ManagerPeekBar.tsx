// @arch arch_MK2Q2.5
import type { PointerEventHandler } from 'react';
import { GripHorizontal, Loader2, Play } from 'lucide-react';

import { Button } from '@/components/ui/button';

export function ManagerPeekBar({
  driverTag,
  runCount,
  acquiring,
  tickBusy,
  onTick,
  onExpand,
  expanded,
  controlsId,
  onPointerDown,
  onPointerMove,
  onPointerUp,
  onPointerCancel,
}: {
  driverTag: string;
  runCount: number;
  acquiring: boolean;
  tickBusy: boolean;
  onTick: () => void;
  onExpand: () => void;
  expanded: boolean;
  controlsId: string;
  onPointerDown: PointerEventHandler<HTMLButtonElement>;
  onPointerMove: PointerEventHandler<HTMLButtonElement>;
  onPointerUp: PointerEventHandler<HTMLButtonElement>;
  onPointerCancel: PointerEventHandler<HTMLButtonElement>;
}) {
  return (
    <div className="flex h-full items-center gap-3 px-3 md:px-4">
      <button
        type="button"
        className="flex h-11 w-10 shrink-0 touch-none items-center justify-center rounded-md text-muted-foreground hover:bg-muted md:h-7"
        aria-label="Resize manager"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerCancel}
        onDoubleClick={onExpand}
      >
        <GripHorizontal className="size-4" />
      </button>
      <button
        type="button"
        className="flex min-w-0 flex-1 items-center gap-3 text-left"
        aria-expanded={expanded}
        aria-controls={controlsId}
        aria-label={`${driverTag} manager, ${runCount} active run${runCount === 1 ? '' : 's'}`}
        onClick={onExpand}
      >
        <span
          className={`size-2.5 shrink-0 rounded-full ${
            acquiring ? 'animate-pulse bg-teal-500' : 'bg-muted-foreground/45'
          }`}
          aria-hidden="true"
        />
        <span className="rounded-md border bg-muted px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
          {driverTag}
        </span>
        <span className="rounded-full border px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground">
          {runCount}
        </span>
      </button>
      <Button
        type="button"
        size="sm"
        disabled={tickBusy}
        onClick={onTick}
        aria-label="Open manager"
      >
        {tickBusy ? <Loader2 className="size-3.5 animate-spin" /> : <Play className="size-3.5" />}
        <span className="hidden sm:inline">Manager</span>
      </Button>
    </div>
  );
}
