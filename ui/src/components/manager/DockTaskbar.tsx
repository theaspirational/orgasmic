import type { PointerEvent, ReactNode } from 'react';
import { Bot, ChevronDown, Loader2, Plus, Power, SquareTerminal, X } from 'lucide-react';

import { Button } from '@/components/ui/button';
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from '@/components/ui/context-menu';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';

// One taskbar entry: a live run (manager, terminal, or worker), or a stale tab
// whose run has ended but which the user has not dismissed yet.
export type TaskbarRunButton = {
  tabId: string;
  runId: string;
  kind: 'manager' | 'terminal' | 'worker' | 'stale';
  label: string;
  /** Tooltip detail: full provider-qualified title plus the run id. */
  title: string;
  subState?: string | null;
};

function buttonIcon(kind: TaskbarRunButton['kind']) {
  if (kind === 'terminal') return <SquareTerminal className="size-3.5" />;
  if (kind === 'stale') return <X className="size-3.5" />;
  return <Bot className="size-3.5" />;
}

// The Windows-style run button: icon + short label, with an underline
// indicator that reads "running" (short, muted) or "in front" (wide, primary).
function TaskbarButton({
  icon,
  label,
  title,
  active,
  running,
  stale,
  onClick,
  onClose,
  closeLabel,
  menu,
  ariaLabel,
}: {
  icon: ReactNode;
  label: string;
  title: ReactNode;
  active: boolean;
  running: boolean;
  stale?: boolean;
  onClick: () => void;
  /** Renders the inline close affordance. What closing means is the caller's
   * to decide: ending a terminal, or dismissing a dead tab. */
  onClose?: () => void;
  closeLabel?: string;
  menu?: ReactNode;
  ariaLabel?: string;
}) {
  // A div rather than a <button>: the close control is a real nested button, and
  // button-inside-button is invalid DOM (the same reason ProjectTabItem is a div).
  const button = (
    <div
      role="button"
      tabIndex={0}
      data-taskbar-control
      aria-label={ariaLabel ?? label}
      aria-pressed={active}
      onClick={onClick}
      onKeyDown={(event) => {
        if (event.key !== 'Enter' && event.key !== ' ') return;
        event.preventDefault();
        onClick();
      }}
      className={cn(
        'group relative flex h-9 shrink-0 cursor-pointer select-none items-center gap-1.5 rounded-md px-2.5 pb-1 text-xs font-medium outline-none transition-colors focus-visible:ring-2 focus-visible:ring-ring',
        active
          ? 'bg-accent text-accent-foreground'
          : 'text-muted-foreground hover:bg-muted hover:text-foreground',
        stale && 'opacity-60',
      )}
    >
      {icon}
      <span className="max-w-28 truncate sm:max-w-36">{label}</span>
      {onClose ? (
        <button
          type="button"
          tabIndex={-1}
          aria-label={closeLabel ?? `Close ${label}`}
          onClick={(event) => {
            // Closing must not also raise the run behind it.
            event.stopPropagation();
            onClose();
          }}
          // The bar's top edge is a resize grab zone; keep the drag off this hit
          // target so a close press never reads as the start of a resize.
          onPointerDown={(event) => event.stopPropagation()}
          className={cn(
            '-mr-1 flex size-4 shrink-0 items-center justify-center rounded text-muted-foreground opacity-0 transition-opacity hover:bg-foreground/10 hover:text-foreground focus-visible:opacity-100 group-hover:opacity-100 group-focus-within:opacity-100 motion-reduce:transition-none',
            // Touch has no hover to reveal it, and the raised tab is the one you
            // are most likely to be done with.
            active && 'opacity-70',
          )}
        >
          <X className="size-3" />
        </button>
      ) : null}
      {running ? (
        <span
          aria-hidden
          className={cn(
            'absolute inset-x-0 bottom-1 mx-auto h-0.5 rounded-full transition-all duration-200 motion-reduce:transition-none',
            active
              ? 'w-6 bg-primary'
              : 'w-3 bg-muted-foreground/40 group-hover:bg-muted-foreground/70',
          )}
        />
      ) : null}
    </div>
  );

  // Trigger order matters: both Radix triggers use asChild, so they must
  // compose down onto the real <button> — the ContextMenu root itself renders
  // no DOM node and would silently drop the tooltip's handlers if nested the
  // other way around.
  const withTooltip = (trigger: ReactNode) => (
    <Tooltip>
      <TooltipTrigger asChild>{trigger}</TooltipTrigger>
      <TooltipContent side="top" className="font-mono text-[10px]">
        {title}
      </TooltipContent>
    </Tooltip>
  );

  if (!menu) return withTooltip(button);
  return (
    <ContextMenu>
      {withTooltip(<ContextMenuTrigger asChild>{button}</ContextMenuTrigger>)}
      {menu}
    </ContextMenu>
  );
}

export function DockTaskbar({
  open,
  readOnly,
  terminalBusy,
  buttons,
  activeTabId,
  onTerminalLaunch,
  onSelect,
  onStop,
  onDismiss,
  onMinimize,
  resizeHandlers,
  runningAgents,
}: {
  /** Open = a session panel is showing above this bar. */
  open: boolean;
  readOnly: boolean;
  terminalBusy: boolean;
  buttons: TaskbarRunButton[];
  activeTabId: string | null;
  onTerminalLaunch: () => void;
  onSelect: (button: TaskbarRunButton) => void;
  onStop: (button: TaskbarRunButton) => void;
  onDismiss: (button: TaskbarRunButton) => void;
  onMinimize: () => void;
  /** Pointer handlers for the top-border drag. They must all sit on this same
   * element: pointer capture retargets move/up to whatever captured the down. */
  resizeHandlers: {
    onPointerDown: (event: PointerEvent<HTMLElement>) => void;
    onPointerMove: (event: PointerEvent<HTMLElement>) => void;
    onPointerUp: (event: PointerEvent<HTMLElement>) => void;
    onPointerCancel: (event: PointerEvent<HTMLElement>) => void;
  };
  runningAgents: ReactNode;
}) {
  return (
    <div
      className={cn(
        'flex h-12 shrink-0 touch-none items-center gap-1 px-1.5 border-t',
        // The bar's own top edge is the resize grab zone whenever a panel is
        // open; collapsed, there is nothing above it to resize.
        open && 'cursor-ns-resize',
      )}
      {...(open ? resizeHandlers : null)}
    >
      <div className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto">
        {/* The tab strip only ever holds tabs; the launcher rides alongside it so
            it lands after the last run rather than in the tablist. */}
        <div className="flex items-center gap-1" role="tablist" aria-label="Open runs">
          {buttons.map((button) => {
            const active = button.tabId === activeTabId && open;
            // The inline x closes what closing can mean for this tab: a dead tab
            // is dismissed, a terminal is ended. A worker or manager has no x —
            // stopping an agent mid-task stays a deliberate context-menu act
            // (dec_FBBT2 keeps stop separate from putting a session away).
            const close =
              button.kind === 'stale'
                ? { onClose: () => onDismiss(button), closeLabel: `Dismiss ${button.label}` }
                : button.kind === 'terminal' && !readOnly
                  ? { onClose: () => onStop(button), closeLabel: `End ${button.label}` }
                  : null;
            const stoppable = !readOnly && (button.kind === 'manager' || button.kind === 'worker');
            return (
              <TaskbarButton
                key={button.tabId}
                icon={buttonIcon(button.kind)}
                label={button.label}
                title={button.title}
                active={active}
                running={button.kind !== 'stale'}
                stale={button.kind === 'stale'}
                onClick={() => onSelect(button)}
                {...close}
                menu={
                  <ContextMenuContent>
                    <ContextMenuItem onSelect={() => onSelect(button)}>
                      {active ? 'Minimize' : 'Bring to front'}
                    </ContextMenuItem>
                    {stoppable ? (
                      <>
                        <ContextMenuSeparator />
                        <ContextMenuItem variant="destructive" onSelect={() => onStop(button)}>
                          <Power className="size-3.5" /> Stop run
                        </ContextMenuItem>
                      </>
                    ) : null}
                  </ContextMenuContent>
                }
              />
            );
          })}
        </div>

        {/* A member without sessions.interact may watch runs but never start
            one, so the launcher is theirs to lose. */}
        {!readOnly ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="button"
                variant="ghost"
                size="icon-sm"
                data-taskbar-control
                aria-label="Open terminal session"
                disabled={terminalBusy}
                onClick={onTerminalLaunch}
                className="shrink-0 text-muted-foreground"
              >
                {terminalBusy ? <Loader2 className="animate-spin" /> : <Plus />}
              </Button>
            </TooltipTrigger>
            <TooltipContent side="top">Open a bare terminal session</TooltipContent>
          </Tooltip>
        ) : null}
      </div>

      <div className="ml-auto flex shrink-0 items-center gap-0.5" data-taskbar-control>
        {runningAgents}
        {open ? (
          <>
            <span className="mx-1 h-5 w-px bg-border" aria-hidden />
            <Button
              type="button"
              variant="ghost"
              size="icon-sm"
              aria-label="Minimize dock"
              title="Minimize dock"
              onClick={onMinimize}
            >
              <ChevronDown />
            </Button>
          </>
        ) : null}
      </div>
    </div>
  );
}
