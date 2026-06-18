import { useMemo, useState } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { ChevronDown, ChevronRight } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
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
import { routeSearch, searchList, type AppSearch } from '@/lib/searchState';
import type { TxRecord } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { ErrorPanel } from './Primitives';

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

function dayKey(record: TxRecord): string {
  const date = parseTime(record.entry.time);
  if (!date) return 'Unknown';
  return date.toISOString().slice(0, 10);
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
  const days = preset === 'today' ? 1 : preset === '7d' ? 7 : 30;
  return Date.now() - date.getTime() <= days * 86_400_000;
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
      const key = dayKey(record);
      map.set(key, [...(map.get(key) ?? []), record]);
    }
    return Array.from(map.entries());
  }, [filtered]);

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

  if (tx.error) return <ErrorPanel error={tx.error} />;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold tracking-tight">Activity</h2>
          <p className="text-sm text-muted-foreground">Recent tx feed for {projectId}.</p>
        </div>
        <Badge variant="outline" className="font-mono">{filtered.length}</Badge>
      </div>
      <Card size="sm">
        <CardContent className="flex flex-col gap-3">
          <div className="flex flex-wrap gap-2">
            {observedTypes.slice(0, 12).map((type) => (
              <Button
                key={type}
                type="button"
                variant={types.includes(type) ? 'secondary' : 'outline'}
                size="sm"
                aria-pressed={types.includes(type)}
                aria-controls={ACTIVITY_FEED_ID}
                onClick={() => toggle('types', types, type)}
              >
                {type}
              </Button>
            ))}
          </div>
          <div className="flex flex-wrap items-center gap-2">
            {observedActors.slice(0, 8).map((actor) => (
              <Button
                key={actor}
                type="button"
                variant={actors.includes(actor) ? 'secondary' : 'outline'}
                size="sm"
                aria-pressed={actors.includes(actor)}
                aria-controls={ACTIVITY_FEED_ID}
                onClick={() => toggle('actors', actors, actor)}
              >
                {actor}
              </Button>
            ))}
            {(['today', '7d', '30d', 'custom', 'all'] as DatePreset[]).map((option) => (
              <Button
                key={option}
                type="button"
                variant={preset === option ? 'secondary' : 'outline'}
                size="sm"
                aria-pressed={preset === option}
                aria-controls={ACTIVITY_FEED_ID}
                onClick={() => setRange(option)}
              >
                {option}
              </Button>
            ))}
            {preset === 'custom' ? (
              <div className="flex flex-wrap items-center gap-2">
                <Input
                  type="date"
                  value={customFrom}
                  onChange={(event) => setCustomDate('from', event.target.value)}
                  className="h-8 w-[9.5rem]"
                  aria-label="Activity from date"
                />
                <Input
                  type="date"
                  value={customTo}
                  onChange={(event) => setCustomDate('to', event.target.value)}
                  className="h-8 w-[9.5rem]"
                  aria-label="Activity to date"
                />
              </div>
            ) : null}
            <Select value={task} onValueChange={setTask}>
              <SelectTrigger size="sm" className="w-[12rem]">
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
          </div>
        </CardContent>
      </Card>
      <div id={ACTIVITY_FEED_ID} role="region" aria-label="Activity feed" aria-busy={tx.loading} className="flex flex-col gap-4">
        {tx.loading && !tx.data ? (
          <div className="flex flex-col gap-3">
            {Array.from({ length: 6 }).map((_, index) => (
              <Card key={index} size="sm">
                <CardHeader>
                  <Skeleton className="h-4 w-2/3" />
                </CardHeader>
                <CardContent className="flex flex-col gap-2">
                  <Skeleton className="h-3 w-1/2" />
                  <Skeleton className="h-3 w-3/4" />
                </CardContent>
              </Card>
            ))}
          </div>
        ) : groups.length === 0 ? (
          <Card>
            <CardContent className="px-6 py-10 text-center text-sm text-muted-foreground">
              No activity in this filter.
            </CardContent>
          </Card>
        ) : (
          groups.map(([key, records]) => (
            <section key={key} className="flex flex-col gap-2">
              <h3 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">{dayLabel(key)}</h3>
              {collapse(records).map((cluster) => (
                <ActivityCard
                  key={cluster.key}
                  cluster={cluster}
                  onOpenTask={(taskId) => {
                    void navigate({
                      to: '/projects/$projectId/tasks',
                      params: { projectId },
                      search: routeSearch((prev) => ({
                        ...prev,
                        task: taskId,
                      })),
                    });
                  }}
                />
              ))}
            </section>
          ))
        )}
      </div>
      <Button type="button" variant="outline" onClick={() => setLimit((current) => current + 200)}>
        Load more
      </Button>
    </div>
  );
}

function ActivityCard({
  cluster,
  onOpenTask,
}: {
  cluster: Cluster;
  onOpenTask: (taskId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const head = cluster.head.entry;
  const count = cluster.entries.length - 1;
  return (
    <Card size="sm">
      <CardHeader className="gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <Button type="button" variant="ghost" size="icon-sm" onClick={() => setOpen((value) => !value)}>
            {open ? <ChevronDown /> : <ChevronRight />}
          </Button>
          <CardTitle className="min-w-0 flex-1 truncate text-sm">{head.reason || head.ty}</CardTitle>
          <Badge variant="outline" className="font-mono">{head.ty}</Badge>
        </div>
      </CardHeader>
      <CardContent className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
          <code className="font-mono">{head.tx_id}</code>
          <span>{head.actor}</span>
          <span>{head.time}</span>
          {head.task ? (
            <Button type="button" variant="outline" size="sm" onClick={() => onOpenTask(head.task!)}>
              {head.task}
            </Button>
          ) : null}
          {count > 0 ? <Badge variant="secondary">+{count} follow-up</Badge> : null}
          {count > 0 ? <span>last {cluster.last.entry.time}</span> : null}
        </div>
        {open ? (
          <div className="flex flex-col gap-2 border-t pt-3">
            {cluster.entries.map((record) => (
              <div key={record.entry.tx_id} className="rounded-md border bg-muted/20 px-3 py-2 text-xs">
                <div className="flex flex-wrap gap-2 text-muted-foreground">
                  <code className="font-mono">{record.entry.tx_id}</code>
                  <span>{record.entry.ty}</span>
                  <span>{record.entry.time}</span>
                </div>
                <p className="mt-1 whitespace-pre-wrap">{record.entry.reason || record.entry.target || '—'}</p>
              </div>
            ))}
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}
