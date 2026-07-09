// @arch arch_MK2Q2.7
import type { ReactNode } from 'react';
import { Search } from 'lucide-react';

import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { cn } from '@/lib/utils';

export function NodeListView<T>({
  ariaLabel,
  items,
  getId,
  renderRow,
  renderActionZone,
  filters,
  onSelect,
  search,
  onSearchChange,
  loading = false,
  listId,
  emptyLabel = 'No nodes.',
}: {
  /** Accessible name for the list region (the visible page title lives in PageHeader). */
  ariaLabel: string;
  items: T[];
  getId: (item: T) => string;
  renderRow: (item: T) => ReactNode;
  renderActionZone?: (item: T) => ReactNode;
  filters?: ReactNode;
  onSelect?: (id: string) => void;
  search?: string;
  onSearchChange?: (value: string) => void;
  loading?: boolean;
  listId?: string;
  emptyLabel?: ReactNode;
}) {
  return (
    <div className="flex flex-col gap-4">
      {(onSearchChange || filters) ? (
        <div className="sticky top-0 z-10 -mx-4 border-b bg-background/85 px-4 pb-3 backdrop-blur">
          <div className="flex flex-col gap-3 md:flex-row md:items-center">
            {onSearchChange ? (
              <label className="relative w-full md:max-w-xs">
                <Search className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
                <Input
                  value={search ?? ''}
                  onChange={(event) => onSearchChange(event.target.value)}
                  placeholder="Search"
                  className="pl-8"
                />
              </label>
            ) : null}
            {filters ? <div className="flex flex-wrap items-center gap-1.5">{filters}</div> : null}
          </div>
        </div>
      ) : null}
      <div
        id={listId}
        role="region"
        aria-label={ariaLabel}
        aria-busy={loading}
        className="overflow-hidden rounded-lg border bg-card/45 backdrop-blur-md"
      >
        {loading ? (
          <div className="flex flex-col gap-3 p-4">
            {Array.from({ length: 6 }).map((_, index) => (
              <div key={index} className="grid gap-2 md:grid-cols-[7rem_1fr_auto] md:items-center">
                <Skeleton className="h-4 w-20" />
                <div className="flex flex-col gap-2">
                  <Skeleton className="h-4 w-3/4" />
                  <Skeleton className="h-3 w-1/2" />
                </div>
                <Skeleton className="hidden h-6 w-24 md:block" />
              </div>
            ))}
          </div>
        ) : items.length === 0 ? (
          <div className="px-6 py-12 text-center text-sm text-muted-foreground">{emptyLabel}</div>
        ) : (
          <ul className="divide-y">
            {items.map((item) => {
              const id = getId(item);
              const actionZone = renderActionZone?.(item);
              return (
                <li key={id} className="relative">
                  <div
                    role={onSelect ? 'button' : undefined}
                    tabIndex={onSelect ? 0 : undefined}
                    onClick={() => onSelect?.(id)}
                    onKeyDown={(event) => {
                      if ((event.key === 'Enter' || event.key === ' ') && onSelect) {
                        event.preventDefault();
                        onSelect(id);
                      }
                    }}
                    className={cn(
                      'flex w-full items-center px-4 py-3 text-left transition-colors focus-visible:bg-muted/40 focus-visible:outline-none',
                      onSelect && 'hover:bg-muted/40',
                      actionZone && 'pr-14 sm:pr-44',
                    )}
                  >
                    {renderRow(item)}
                  </div>
                  {actionZone ? (
                    <div
                      className="absolute right-2 top-1/2 flex -translate-y-1/2 items-center gap-1.5"
                      onPointerDown={(event) => event.stopPropagation()}
                      onClick={(event) => event.stopPropagation()}
                    >
                      {actionZone}
                    </div>
                  ) : null}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </div>
  );
}
