import { Activity, ArrowUpRight, FolderGit2, GitBranch, ListChecks } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchProjects } from '@/lib/api';
import type { ProjectIndex } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { ErrorPanel } from './Primitives';

type BadgeVariant = 'default' | 'secondary' | 'destructive' | 'outline';

function statusVariant(status: string): BadgeVariant {
  if (status === 'active') return 'default';
  if (status === 'paused') return 'secondary';
  return 'outline';
}

function relativeTime(iso?: string | null): string {
  if (!iso) return '—';
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return iso;
  const dt = Date.now() - t;
  const m = Math.floor(dt / 60_000);
  if (m < 1) return 'just now';
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  const d = Math.floor(h / 24);
  return `${d}d ago`;
}

type Stats = {
  total: number;
  done: number;
  active: number;
  blocked: number;
};

function hasBlockedBy(blockedBy: ProjectIndex['tasks'][number]['blocked_by']): boolean {
  if (Array.isArray(blockedBy)) return blockedBy.some((item) => item.trim());
  return typeof blockedBy === 'string' && Boolean(blockedBy.trim());
}

function projectStats(p: ProjectIndex): Stats {
  const s: Stats = { total: p.tasks.length, done: 0, active: 0, blocked: 0 };
  for (const t of p.tasks) {
    if (t.lifecycle_stage === 'done' || t.lifecycle_stage === 'cancelled') s.done++;
    else {
      s.active++;
      if (hasBlockedBy(t.blocked_by)) s.blocked++;
    }
  }
  return s;
}

export function BoardView({
  onSelectProject,
}: {
  onSelectProject: (projectId: string) => void;
}) {
  const refresh = useRefreshToken();
  const { data, error, loading } = useResource(`projects:${refresh}`, fetchProjects);

  if (error) return <ErrorPanel error={error} />;

  const projects: ProjectIndex[] = data ?? [];

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-baseline justify-between gap-4">
        <div>
          <h2 className="text-lg font-semibold tracking-tight">Board</h2>
          <p className="text-sm text-muted-foreground">
            Projects registered on this daemon.
          </p>
        </div>
        <Badge variant="outline" className="font-mono">
          {projects.length} project{projects.length === 1 ? '' : 's'}
        </Badge>
      </div>

      {loading && !data ? (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          {[0, 1, 2].map((i) => (
            <Skeleton key={i} className="h-44" />
          ))}
        </div>
      ) : projects.length === 0 ? (
        <Card>
          <CardHeader>
            <FolderGit2 className="size-6 text-muted-foreground" />
            <CardTitle>No projects yet</CardTitle>
            <CardDescription>
              Register one with{' '}
              <code className="font-mono text-foreground">orgasmic project init</code> to see it here.
            </CardDescription>
          </CardHeader>
        </Card>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 xl:grid-cols-3">
          {projects.map((p) => {
            const s = projectStats(p);
            return (
              <Card
                key={p.project_id}
                role="button"
                tabIndex={0}
                onClick={() => onSelectProject(p.project_id)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    onSelectProject(p.project_id);
                  }
                }}
                className="cursor-pointer transition-colors hover:border-primary/60 focus-visible:border-primary/60 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40"
              >
                <CardHeader>
                  <CardTitle className="truncate font-mono text-base">{p.project_id}</CardTitle>
                  <CardDescription className="truncate font-mono text-xs">
                    {p.repo_url}
                  </CardDescription>
                  <CardAction>
                    <Badge variant={statusVariant(p.status)} className="capitalize">
                      {p.status}
                    </Badge>
                  </CardAction>
                </CardHeader>
                <CardContent>
                  <div className="flex flex-wrap items-center gap-x-4 gap-y-2 text-xs text-muted-foreground">
                    <span className="inline-flex items-center gap-1.5">
                      <GitBranch className="size-3.5" />
                      <span className="font-mono">{p.branch}</span>
                    </span>
                    <span className="inline-flex items-center gap-1.5">
                      <ListChecks className="size-3.5" />
                      {s.total} task{s.total === 1 ? '' : 's'}
                    </span>
                    <span className="inline-flex items-center gap-1.5">
                      <Activity className="size-3.5" />
                      {relativeTime(p.last_loaded_at)}
                    </span>
                  </div>
                  <div className="mt-3 flex flex-wrap gap-1.5">
                    {s.active > 0 ? (
                      <Badge variant="default" className="font-mono text-[10px]">
                        {s.active} active
                      </Badge>
                    ) : null}
                    {s.blocked > 0 ? (
                      <Badge variant="destructive" className="font-mono text-[10px]">
                        {s.blocked} blocked
                      </Badge>
                    ) : null}
                    {s.done > 0 ? (
                      <Badge variant="secondary" className="font-mono text-[10px]">
                        {s.done} done
                      </Badge>
                    ) : null}
                  </div>
                </CardContent>
                <CardFooter className="flex items-center justify-between gap-3 border-t pt-3 text-xs text-muted-foreground">
                  <span className="truncate font-mono">{p.root}</span>
                  <ArrowUpRight className="size-4 shrink-0" />
                </CardFooter>
              </Card>
            );
          })}
        </div>
      )}
    </div>
  );
}
