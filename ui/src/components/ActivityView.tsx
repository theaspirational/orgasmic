import { useMemo, useState, type ReactNode } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { ChevronDown, ChevronRight, Clock3, ListFilter, UserRound, X } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent } from '@/components/ui/card';
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { Input } from '@/components/ui/input';
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Skeleton } from '@/components/ui/skeleton';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchTx } from '@/lib/api';
import { withEntityPeek } from '@/lib/entityPeek';
import { routeSearch, searchList, type AppSearch } from '@/lib/searchState';
import type { TxRecord } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { ErrorPanel, PageHeader } from './Primitives';

type Cluster = {
  key: string;
  head: TxRecord;
  entries: TxRecord[];
  last: TxRecord;
};

type DatePreset = 'today' | '7d' | '30d' | 'custom' | 'all';
type ActivitySearch = AppSearch & {
  types?: string[];
  actors?: string[];
  range?: DatePreset;
  from?: string;
  to?: string;
  task?: string;
};

const ACTIVITY_FEED_ID = 'activity-feed-region';

const COMMON_TYPES = [
  'comment',
  'manager.action',
  'run.created',
  'run.failed',
];

const EVENT_TYPE_LABELS: Record<string, string> = {
  'benchmark.concurrent_control': 'Concurrency benchmark',
  comment: 'Comment',
  'implementer.done': 'Implementation completed',
  'implementer.reported': 'Implementation reported',
  'manager.action': 'Manager action',
  'manager.dispatch_aborted': 'Dispatch aborted',
  'manager.dispatch_orphaned': 'Dispatch orphaned',
  'manager.dispatch_started': 'Dispatch started',
  'manager.tier': 'Priority updated',
  'reviewer.done': 'Review completed',
  'reviewer.reported': 'Review reported',
  'run.created': 'Run created',
  'run.failed': 'Run failed',
  'task.created': 'Task created',
  'task.property_updated': 'Task updated',
  'task.state_transitioned': 'Task status changed',
};

function parseTime(value: string): Date | null {
  const match = /\[(\d{4})-(\d{2})-(\d{2})(?:[^\]]*?(\d{2}):(\d{2})(?::(\d{2}))?)?\]/.exec(value);
  if (!match) return null;
  return new Date(
    Number(match[1]),
    Number(match[2]) - 1,
    Number(match[3]),
    Number(match[4] ?? '0'),
    Number(match[5] ?? '0'),
    Number(match[6] ?? '0'),
  );
}

export function activityDayKey(record: TxRecord): string {
  const date = parseTime(record.entry.time);
  if (!date) return 'Unknown';
  const month = String(date.getMonth() + 1).padStart(2, '0');
  const day = String(date.getDate()).padStart(2, '0');
  return `${date.getFullYear()}-${month}-${day}`;
}

function dayLabel(key: string): string {
  if (key === 'Unknown') return key;
  const date = new Date(`${key}T00:00:00`);
  const today = new Date();
  const start = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  const diff = Math.round((start.getTime() - date.getTime()) / 86_400_000);
  if (diff === 0) return 'Today';
  if (diff === 1) return 'Yesterday';
  return date.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' });
}

function inPreset(record: TxRecord, preset: DatePreset, customFrom: string, customTo: string): boolean {
  if (preset === 'all') return true;
  const date = parseTime(record.entry.time);
  if (!date) return true;
  if (preset === 'today') {
    const today = new Date();
    return (
      date.getFullYear() === today.getFullYear() &&
      date.getMonth() === today.getMonth() &&
      date.getDate() === today.getDate()
    );
  }
  if (preset === 'custom') {
    if (customFrom) {
      const from = new Date(`${customFrom}T00:00:00`);
      if (date < from) return false;
    }
    if (customTo) {
      const to = new Date(`${customTo}T23:59:59`);
      if (date > to) return false;
    }
    return true;
  }
  const days = preset === '7d' ? 7 : 30;
  return Date.now() - date.getTime() <= days * 86_400_000;
}

export function humanizeActivityType(value: string): string {
  const known = EVENT_TYPE_LABELS[value];
  if (known) return known;
  const words = value
    .split(/[._-]+/)
    .filter(Boolean)
    .map((word) => (word.toLowerCase() === 'tx' ? 'transaction' : word.toLowerCase()));
  if (words.length === 0) return 'Activity';
  const label = words.join(' ');
  return label.charAt(0).toUpperCase() + label.slice(1);
}

function humanizeActor(value: string): string {
  if (value === 'agent.implementer') return 'Implementer agent';
  if (value === 'agent.reviewer') return 'Reviewer agent';
  return value;
}

function humanizeStatus(value: string): string {
  const label = value.replaceAll('_', ' ').trim();
  return label ? label.charAt(0).toUpperCase() + label.slice(1) : value;
}

export function activitySummary(entry: TxRecord['entry']): string {
  const raw = (entry.reason || entry.target || '').trim();
  if (!raw || raw === entry.ty) return humanizeActivityType(entry.ty);

  const transition = /^transition\s+(\S+)\s+to\s+(\S+)$/i.exec(raw);
  if (transition) return `${transition[1]} moved to ${humanizeStatus(transition[2])}`;

  const propertyUpdate = /^update properties on\s+(\S+)$/i.exec(raw);
  if (propertyUpdate) return `Updated ${propertyUpdate[1]}`;

  return raw;
}

function formatActivityTime(value: string): string {
  const date = parseTime(value);
  if (!date) return value.replace(/^\[|\]$/g, '');
  return date.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit' });
}

function formatActivityDateTime(value: string): string {
  const date = parseTime(value);
  if (!date) return value.replace(/^\[|\]$/g, '');
  return date.toLocaleString(undefined, {
    weekday: 'short',
    month: 'short',
    day: 'numeric',
    year: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
  });
}

function activityTypeClass(type: string): string {
  if (/failed|aborted|orphaned/.test(type)) {
    return 'border-destructive/30 bg-destructive/10 text-destructive';
  }
  if (/done|reported/.test(type)) {
    return 'border-primary/25 bg-primary/10 text-primary';
  }
  return 'bg-muted/50 text-muted-foreground';
}

function collapse(records: TxRecord[]): Cluster[] {
  const clusters: Cluster[] = [];
  for (const record of records) {
    const previous = clusters.at(-1);
    const recordTime = parseTime(record.entry.time)?.getTime() ?? 0;
    const lastTime = previous ? parseTime(previous.last.entry.time)?.getTime() ?? 0 : 0;
    if (
      previous &&
      previous.head.entry.task &&
      previous.head.entry.task === record.entry.task &&
      Math.abs(recordTime - lastTime) <= 5 * 60_000
    ) {
      previous.entries.push(record);
      previous.last = record;
    } else {
      clusters.push({
        key: record.entry.tx_id,
        head: record,
        entries: [record],
        last: record,
      });
    }
  }
  return clusters;
}

export function ActivityView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as ActivitySearch;
  const refresh = useRefreshToken();
  const [limit, setLimit] = useState(200);
  const types = useMemo(() => searchList(search.types), [search.types]);
  const actors = useMemo(() => searchList(search.actors), [search.actors]);
  const preset = search.range ?? '30d';
  const customFrom = search.from ?? '';
  const customTo = search.to ?? '';
  const task = search.task ?? 'all';
  const tx = useResource(`activity:${projectId}:${limit}:${refresh}`, () => fetchTx(projectId, limit));
  const observedTypes = useMemo(() => {
    const set = new Set([...COMMON_TYPES, ...(tx.data ?? []).map((record) => record.entry.ty)]);
    return Array.from(set).filter(Boolean).sort();
  }, [tx.data]);
  const observedActors = useMemo(() => {
    return Array.from(new Set((tx.data ?? []).map((record) => record.entry.actor).filter(Boolean))).sort();
  }, [tx.data]);
  const observedTasks = useMemo(() => {
    return Array.from(new Set((tx.data ?? []).map((record) => record.entry.task).filter(Boolean) as string[])).sort();
  }, [tx.data]);
  const filtered = useMemo(() => {
    return [...(tx.data ?? [])]
      .sort((a, b) => (parseTime(b.entry.time)?.getTime() ?? 0) - (parseTime(a.entry.time)?.getTime() ?? 0))
      .filter((record) => {
        if (types.length > 0 && !types.includes(record.entry.ty)) return false;
        if (actors.length > 0 && !actors.includes(record.entry.actor)) return false;
        if (task !== 'all' && record.entry.task !== task) return false;
        return inPreset(record, preset, customFrom, customTo);
      });
  }, [actors, customFrom, customTo, preset, task, tx.data, types]);
  const groups = useMemo(() => {
    const map = new Map<string, TxRecord[]>();
    for (const record of filtered) {
      const key = activityDayKey(record);
      map.set(key, [...(map.get(key) ?? []), record]);
    }
    return Array.from(map.entries());
  }, [filtered]);
  const hasActiveFilters =
    types.length > 0 ||
    actors.length > 0 ||
    preset !== '30d' ||
    customFrom !== '' ||
    customTo !== '' ||
    task !== 'all';

  function toggle(key: 'types' | 'actors', list: string[], value: string) {
    const next = list.includes(value) ? list.filter((item) => item !== value) : [...list, value];
    void navigate({
      search: routeSearch((prev) => ({
        ...prev,
        [key]: next.length > 0 ? next : undefined,
      })),
    });
  }

  function setRange(range: DatePreset) {
    void navigate({
      search: routeSearch((prev) => ({
        ...prev,
        range,
        from: range === 'custom' ? prev.from : undefined,
        to: range === 'custom' ? prev.to : undefined,
      })),
    });
  }

  function setCustomDate(key: 'from' | 'to', value: string) {
    void navigate({
      search: routeSearch((prev) => ({
        ...prev,
        range: 'custom',
        [key]: value || undefined,
      })),
      replace: true,
    });
  }

  function setTask(value: string) {
    void navigate({
      search: routeSearch((prev) => ({
        ...prev,
        task: value === 'all' ? undefined : value,
      })),
    });
  }

  function clearFilters() {
    void navigate({
      search: routeSearch((prev) => ({
        ...prev,
        types: undefined,
        actors: undefined,
        range: undefined,
        from: undefined,
        to: undefined,
        task: undefined,
      })),
    });
  }

  if (tx.error) return <ErrorPanel error={tx.error} />;

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Activity"
        description={
          <>
            Recent project changes and agent work.{' '}
            <span className="font-medium text-foreground" aria-live="polite">
              {filtered.length} {filtered.length === 1 ? 'event' : 'events'}
            </span>
          </>
        }
      />
      <Card size="sm">
        <CardContent className="flex flex-col gap-3">
          <div className="grid grid-cols-2 gap-2 lg:grid-cols-[auto_auto_auto_minmax(12rem,1fr)_auto]">
            <MultiSelectFilter
              label="Types"
              icon={<ListFilter />}
              items={observedTypes}
              selected={types}
              formatItem={humanizeActivityType}
              onToggle={(value) => toggle('types', types, value)}
            />
            <MultiSelectFilter
              label="Actors"
              icon={<UserRound />}
              items={observedActors}
              selected={actors}
              formatItem={humanizeActor}
              onToggle={(value) => toggle('actors', actors, value)}
            />
            <Select value={preset} onValueChange={(value) => setRange(value as DatePreset)}>
              <SelectTrigger size="sm" className="w-full" aria-label="Activity date range">
                <Clock3 />
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  <SelectItem value="today">Today</SelectItem>
                  <SelectItem value="7d">7 days</SelectItem>
                  <SelectItem value="30d">30 days</SelectItem>
                  <SelectItem value="custom">Custom</SelectItem>
                  <SelectItem value="all">All time</SelectItem>
                </SelectGroup>
              </SelectContent>
            </Select>
            <Select value={task} onValueChange={setTask}>
              <SelectTrigger size="sm" className="w-full" aria-label="Activity task">
                <SelectValue placeholder="Task" />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  <SelectItem value="all">All tasks</SelectItem>
                  {observedTasks.map((taskId) => (
                    <SelectItem key={taskId} value={taskId}>{taskId}</SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
            {hasActiveFilters ? (
              <Button type="button" variant="ghost" size="sm" className="col-span-2 lg:col-span-1" onClick={clearFilters}>
                <X />
                Clear filters
              </Button>
            ) : null}
          </div>
          {preset === 'custom' ? (
            <div className="grid grid-cols-2 gap-2 sm:max-w-sm">
              <Input
                type="date"
                value={customFrom}
                onChange={(event) => setCustomDate('from', event.target.value)}
                className="h-8"
                aria-label="Activity from date"
              />
              <Input
                type="date"
                value={customTo}
                onChange={(event) => setCustomDate('to', event.target.value)}
                className="h-8"
                aria-label="Activity to date"
              />
            </div>
          ) : null}
        </CardContent>
      </Card>
      <div id={ACTIVITY_FEED_ID} role="region" aria-label="Activity feed" aria-busy={tx.loading} className="flex flex-col gap-6">
        {tx.loading && !tx.data ? (
          <div className="divide-y overflow-hidden rounded-xl bg-card ring-1 ring-foreground/10">
            {Array.from({ length: 6 }).map((_, index) => (
              <div key={index} className="space-y-2 p-4">
                <Skeleton className="h-3 w-1/3" />
                <Skeleton className="h-4 w-3/4" />
                <Skeleton className="h-3 w-1/2" />
              </div>
            ))}
          </div>
        ) : groups.length === 0 ? (
          <Card>
            <CardContent className="flex flex-col items-center gap-3 px-6 py-10 text-center text-sm text-muted-foreground">
              <p>No activity matches these filters.</p>
              {hasActiveFilters ? (
                <Button type="button" variant="outline" size="sm" onClick={clearFilters}>Clear filters</Button>
              ) : null}
            </CardContent>
          </Card>
        ) : (
          groups.map(([key, records]) => {
            const clusters = collapse(records);
            return (
              <section key={key} className="flex flex-col gap-2">
                <div className="flex items-center gap-3">
                  <h3 className="text-sm font-semibold text-foreground">{dayLabel(key)}</h3>
                  <span className="text-xs tabular-nums text-muted-foreground">
                    {records.length} {records.length === 1 ? 'event' : 'events'}
                  </span>
                  <span className="h-px flex-1 bg-border" aria-hidden="true" />
                </div>
                <div className="divide-y overflow-hidden rounded-xl bg-card ring-1 ring-foreground/10">
                  {clusters.map((cluster) => (
                    <ActivityItem
                      key={cluster.key}
                      cluster={cluster}
                      onOpenTask={(taskId) => {
                        void navigate({
                          search: routeSearch((prev) => withEntityPeek(prev, taskId)),
                        });
                      }}
                    />
                  ))}
                </div>
              </section>
            );
          })
        )}
      </div>
      {(tx.data?.length ?? 0) >= limit ? (
        <Button type="button" variant="outline" onClick={() => setLimit((current) => current + 200)}>
          Load 200 more events
        </Button>
      ) : null}
    </div>
  );
}

function MultiSelectFilter({
  label,
  icon,
  items,
  selected,
  formatItem,
  onToggle,
}: {
  label: string;
  icon: ReactNode;
  items: string[];
  selected: string[];
  formatItem: (value: string) => string;
  onToggle: (value: string) => void;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          type="button"
          variant={selected.length > 0 ? 'secondary' : 'outline'}
          size="sm"
          className="w-full justify-between"
          aria-controls={ACTIVITY_FEED_ID}
        >
          <span className="flex min-w-0 items-center gap-1.5">
            {icon}
            <span className="truncate">{label}</span>
          </span>
          {selected.length > 0 ? (
            <Badge variant="outline" className="h-4 min-w-4 px-1 text-[10px] tabular-nums">{selected.length}</Badge>
          ) : (
            <ChevronDown />
          )}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent className="w-72">
        <DropdownMenuLabel>{label}</DropdownMenuLabel>
        {items.map((value) => {
          const display = formatItem(value);
          return (
            <DropdownMenuCheckboxItem
              key={value}
              checked={selected.includes(value)}
              onCheckedChange={() => onToggle(value)}
              className="items-start py-2"
            >
              <span className="min-w-0">
                <span className="block truncate">{display}</span>
                {display !== value ? (
                  <code className="block truncate font-mono text-[10px] text-muted-foreground">{value}</code>
                ) : null}
              </span>
            </DropdownMenuCheckboxItem>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function ActivityItem({
  cluster,
  onOpenTask,
}: {
  cluster: Cluster;
  onOpenTask: (taskId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const head = cluster.head.entry;
  const count = cluster.entries.length - 1;
  const detailsId = `activity-details-${head.tx_id.replace(/[^a-zA-Z0-9_-]/g, '-')}`;
  const toggleLabel = open
    ? `Hide source records for ${head.tx_id}`
    : `Show source records for ${head.tx_id}`;
  return (
    <article className="px-3 py-3 transition-colors hover:bg-muted/20 sm:px-4 sm:py-4">
      <div className="grid grid-cols-[minmax(0,1fr)_auto] gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
            <time
              className="font-medium tabular-nums text-foreground"
              dateTime={parseTime(head.time)?.toISOString()}
              title={formatActivityDateTime(head.time)}
            >
              {formatActivityTime(head.time)}
            </time>
            <span aria-hidden="true">·</span>
            <span title={head.actor}>{humanizeActor(head.actor)}</span>
            <Badge variant="outline" className={`h-5 px-1.5 text-[11px] ${activityTypeClass(head.ty)}`} title={head.ty}>
              {humanizeActivityType(head.ty)}
            </Badge>
          </div>
          <p className="mt-1.5 max-w-[75ch] whitespace-pre-wrap text-sm leading-6 text-foreground">
            {activitySummary(head)}
          </p>
          <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
            {head.task ? (
              <Button
                type="button"
                variant="link"
                size="sm"
                className="h-auto p-0 font-mono text-xs"
                onClick={() => onOpenTask(head.task!)}
              >
                {head.task}
              </Button>
            ) : null}
            {count > 0 ? (
              <span className="font-medium text-foreground/80">+{count} related</span>
            ) : null}
            <code className="break-all font-mono text-[11px]" title="Transaction ID">{head.tx_id}</code>
          </div>
        </div>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          aria-label={toggleLabel}
          aria-expanded={open}
          aria-controls={detailsId}
          onClick={() => setOpen((value) => !value)}
        >
            {open ? <ChevronDown /> : <ChevronRight />}
        </Button>
      </div>
      {open ? (
        <div id={detailsId} className="mt-3 border-t pt-3" aria-label="Source activity records">
          <div className="divide-y">
            {cluster.entries.map((record) => {
              const entry = record.entry;
              return (
                <div key={entry.tx_id} className="grid gap-1 py-3 first:pt-0 last:pb-0 sm:grid-cols-[6.5rem_minmax(0,1fr)] sm:gap-3">
                  <time
                    className="text-xs tabular-nums text-muted-foreground"
                    dateTime={parseTime(entry.time)?.toISOString()}
                    title={formatActivityDateTime(entry.time)}
                  >
                    {formatActivityTime(entry.time)}
                  </time>
                  <div className="min-w-0">
                    <div className="flex flex-wrap gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
                      <code className="font-mono">{entry.ty}</code>
                      <code className="break-all font-mono">{entry.tx_id}</code>
                    </div>
                    <p className="mt-1 whitespace-pre-wrap text-sm leading-5 text-foreground">
                      {activitySummary(entry)}
                    </p>
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      ) : null}
    </article>
  );
}
