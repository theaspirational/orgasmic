import type { CSSProperties, HTMLAttributes, ReactNode } from 'react';
import { ChevronDown, ChevronRight } from 'lucide-react';

import { Button } from '@/components/ui/button';
import { cn } from '@/lib/utils';

import { CopyIdBadge } from './CopyIdBadge';

const MAX_VISUAL_DEPTH = 6;

type NestedTreeIndentStyle = CSSProperties & {
  '--tree-row-left-mobile': string;
  '--tree-row-left-desktop': string;
  '--tree-toggle-left-mobile': string;
  '--tree-toggle-left-desktop': string;
  '--tree-guide-left-mobile': string;
  '--tree-guide-left-desktop': string;
};

type OpenProps = Omit<
  HTMLAttributes<HTMLDivElement>,
  'aria-label' | 'children' | 'className' | 'onClick' | 'onKeyDown' | 'role' | 'tabIndex'
>;

export function NestedTreeRow({
  depth,
  nodeId,
  nodeKind,
  title,
  secondary,
  titleAdornment,
  meta,
  action,
  corner,
  hasChildren = false,
  expanded = false,
  childrenId,
  onToggle,
  toggleLabel,
  onOpen,
  openLabel,
  openProps,
  indent = 'standard',
  titleLines = 1,
  className,
}: {
  depth: number;
  nodeId: string;
  nodeKind: 'decision' | 'task';
  title: ReactNode;
  secondary?: ReactNode;
  titleAdornment?: ReactNode;
  meta?: ReactNode;
  action?: ReactNode;
  corner?: ReactNode;
  hasChildren?: boolean;
  expanded?: boolean;
  childrenId?: string;
  onToggle?: () => void;
  toggleLabel?: string;
  onOpen: () => void;
  openLabel: string;
  openProps?: OpenProps;
  indent?: 'standard' | 'compact';
  titleLines?: 1 | 2;
  className?: string;
}) {
  const visualDepth = Math.min(depth, MAX_VISUAL_DEPTH);
  const parentVisualDepth = Math.max(0, visualDepth - 1);
  const indentStyle: NestedTreeIndentStyle =
    indent === 'compact'
      ? {
          '--tree-row-left-mobile': `${1 + visualDepth * 0.75}rem`,
          '--tree-row-left-desktop': `${1 + visualDepth * 0.75}rem`,
          '--tree-toggle-left-mobile': `${0.125 + visualDepth * 0.75}rem`,
          '--tree-toggle-left-desktop': `${0.125 + visualDepth * 0.75}rem`,
          '--tree-guide-left-mobile': `${0.25 + parentVisualDepth * 0.75}rem`,
          '--tree-guide-left-desktop': `${0.25 + parentVisualDepth * 0.75}rem`,
        }
      : {
          '--tree-row-left-mobile': `${2 + visualDepth * 0.75}rem`,
          '--tree-row-left-desktop': `${2.25 + visualDepth}rem`,
          '--tree-toggle-left-mobile': `${0.375 + visualDepth * 0.75}rem`,
          '--tree-toggle-left-desktop': `${0.625 + visualDepth}rem`,
          '--tree-guide-left-mobile': `${0.875 + parentVisualDepth * 0.75}rem`,
          '--tree-guide-left-desktop': `${1.125 + parentVisualDepth}rem`,
        };

  return (
    <div className={cn('group/tree-row relative w-full', className)} style={indentStyle}>
      {depth > 0 ? (
        <span
          aria-hidden
          className="pointer-events-none absolute top-0 h-[1.375rem] w-4 rounded-bl-sm border-b border-l border-border/70 left-[var(--tree-guide-left-mobile)] sm:left-[var(--tree-guide-left-desktop)]"
        />
      ) : null}
      {hasChildren ? (
        <Button
          type="button"
          variant="ghost"
          size="icon-xs"
          className="absolute top-[0.875rem] z-10 size-5 rounded-sm text-muted-foreground left-[var(--tree-toggle-left-mobile)] sm:left-[var(--tree-toggle-left-desktop)]"
          aria-expanded={expanded}
          aria-controls={childrenId}
          aria-label={toggleLabel ?? `${expanded ? 'Collapse' : 'Expand'} ${nodeId}`}
          onPointerDown={(event) => event.stopPropagation()}
          onClick={(event) => {
            event.stopPropagation();
            onToggle?.();
          }}
        >
          {expanded ? <ChevronDown /> : <ChevronRight />}
        </Button>
      ) : null}
      <CopyIdBadge
        value={nodeId}
        label={`${nodeKind} id ${nodeId}`}
        className="absolute top-0.5 z-10 h-4 origin-top-left scale-[0.6] rounded-sm bg-background px-1 text-[10px] leading-none text-muted-foreground left-[var(--tree-row-left-mobile)] sm:left-[var(--tree-row-left-desktop)]"
      />
      <div className={cn('flex w-full items-start', corner && 'flex-col sm:flex-row')}>
        <div
          {...openProps}
          role="button"
          tabIndex={0}
          aria-label={openLabel}
          onClick={onOpen}
          onKeyDown={(event) => {
            if (event.key !== 'Enter' && event.key !== ' ') return;
            event.preventDefault();
            onOpen();
          }}
          className={cn(
            'flex min-h-11 min-w-0 flex-1 items-start gap-2 pb-1 pt-4 text-left transition-colors hover:bg-muted/40 focus-visible:bg-muted/40 focus-visible:outline-none pl-[var(--tree-row-left-mobile)] sm:pl-[var(--tree-row-left-desktop)]',
            corner ? 'pr-4 sm:pr-2' : 'pr-10',
          )}
        >
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 items-center gap-1.5">
              <span
                className={cn(
                  'min-w-0 text-[13px] font-medium leading-4',
                  titleLines === 2 ? 'line-clamp-2' : 'truncate',
                )}
              >
                {title}
              </span>
              {titleAdornment}
            </div>
            {secondary ? (
              <div className="truncate text-[10px] leading-3.5 text-muted-foreground">
                {secondary}
              </div>
            ) : null}
          </div>
          {meta ? <div className="flex shrink-0 items-center gap-1">{meta}</div> : null}
        </div>
        {corner ? (
          <div className="z-10 flex w-full shrink-0 justify-end pb-1 pl-[var(--tree-row-left-mobile)] pr-4 sm:w-auto sm:pb-0 sm:pl-0 sm:pr-6 sm:pt-1">
            {corner}
          </div>
        ) : null}
      </div>
      {action ? (
        <div
          className="absolute right-2 top-1/2 z-10 flex -translate-y-1/2 items-center"
          onPointerDown={(event) => event.stopPropagation()}
          onClick={(event) => event.stopPropagation()}
        >
          {action}
        </div>
      ) : null}
    </div>
  );
}
