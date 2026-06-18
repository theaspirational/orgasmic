import { useState } from 'react';
import { Eye, FolderTree, GitBranch, Link2, Pencil } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Separator } from '@/components/ui/separator';
import { Skeleton } from '@/components/ui/skeleton';
import { NodeDocEditor, type NodeDirectory } from '@/components/orgdoc/NodeDocEditor';
import { PROJECT_DESCRIPTOR } from '@/components/orgdoc/descriptor';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchProject } from '@/lib/api';
import type { LifecycleStage, ProjectIndex, TaskSummary } from '@/lib/types';
import { LIFECYCLE_STAGES, lifecycleStageLabel } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { cn } from '@/lib/utils';

import { ErrorPanel } from './Primitives';

// project.org sections have no cross-node link chips, so the editor's directory
// is a trivial identity.
const EMPTY_DIRECTORY: NodeDirectory = {
  labelFor: (id) => id,
  suggestionsFor: () => [],
};

type BadgeVariant = 'default' | 'secondary' | 'destructive' | 'outline';

export function stageVariant(stage: LifecycleStage | string): BadgeVariant {
  switch (stage) {
    case 'done':
      return 'secondary';
    case 'in_progress':
    case 'in_review':
      return 'default';
    case 'cancelled':
      return 'destructive';
    default:
      return 'outline';
  }
}

function groupByStage(tasks: TaskSummary[]): Map<LifecycleStage | string, TaskSummary[]> {
  const groups = new Map<LifecycleStage | string, TaskSummary[]>();
  for (const t of tasks) {
    const k = t.lifecycle_stage ?? 'backlog';
    const list = groups.get(k) ?? [];
    list.push(t);
    groups.set(k, list);
  }
  return groups;
}

function orderedStages(groups: Map<string, TaskSummary[]>): string[] {
  const seen = new Set(groups.keys());
  const ordered: string[] = LIFECYCLE_STAGES.filter((s) => seen.has(s));
  for (const k of [...seen].sort()) {
    if (!ordered.includes(k)) ordered.push(k);
  }
  return ordered;
}

export function ProjectView({
  projectId,
  onSelectTask: _onSelectTask,
}: {
  projectId: string;
  onSelectTask: (taskId: string) => void;
}) {
  const refresh = useRefreshToken();
  const { data, error, loading } = useResource(
    `project:${projectId}:${refresh}`,
    () => fetchProject(projectId),
  );

  if (loading && !data) {
    return (
      <div className="flex flex-col gap-4">
        <Skeleton className="h-36" />
        <Skeleton className="h-96" />
      </div>
    );
  }
  if (error) return <ErrorPanel error={error} />;
  if (!data) return null;

  const project: ProjectIndex = data;
  const tasks = project.tasks ?? [];
  const grouped = groupByStage(tasks) as Map<string, TaskSummary[]>;
  const stages = orderedStages(grouped);

  return (
    <div className="flex flex-col gap-4">
      <ProjectHeader project={project} grouped={grouped} stages={stages} />
      <ProjectSections projectId={project.project_id} nodeId={project.project_id} />
    </div>
  );
}

function ProjectSections({ projectId, nodeId }: { projectId: string; nodeId: string }) {
  const [mode, setMode] = useState<'view' | 'edit'>('view');
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Overview</CardTitle>
        <CardDescription>From .orgasmic/project.org</CardDescription>
        <CardAction>
          <Button
            type="button"
            variant={mode === 'edit' ? 'default' : 'outline'}
            size="sm"
            onClick={() => setMode((current) => (current === 'edit' ? 'view' : 'edit'))}
            aria-pressed={mode === 'edit'}
          >
            {mode === 'edit' ? <Eye /> : <Pencil />}
            {mode === 'edit' ? 'View' : 'Edit'}
          </Button>
        </CardAction>
      </CardHeader>
      <CardContent>
        <NodeDocEditor
          projectId={projectId}
          nodeId={nodeId}
          descriptor={PROJECT_DESCRIPTOR}
          directory={EMPTY_DIRECTORY}
          onOpenNode={() => {}}
          mode={mode}
          apiKind="project"
        />
      </CardContent>
    </Card>
  );
}

function ProjectHeader({
  project,
  grouped,
  stages,
}: {
  project: ProjectIndex;
  grouped: Map<string, TaskSummary[]>;
  stages: string[];
}) {
  return (
    <Card>
      <CardHeader className="gap-1.5 pr-24">
        <CardTitle className="min-w-0 [overflow-wrap:anywhere] font-mono text-base">{project.project_id}</CardTitle>
        <CardDescription className="min-w-0 [overflow-wrap:anywhere] font-mono text-xs">
          {project.repo_url}
        </CardDescription>
        <CardAction>
          <Badge
            variant={project.status === 'active' ? 'default' : 'secondary'}
            className="capitalize"
          >
            {project.status}
          </Badge>
        </CardAction>
      </CardHeader>
      <CardContent>
        <dl className="grid grid-cols-1 gap-3 lg:grid-cols-[minmax(8rem,12rem)_minmax(16rem,1fr)_minmax(18rem,1.2fr)]">
          <Field icon={GitBranch} label="Branch" value={project.branch} mono />
          <Field icon={Link2} label="Repository" value={project.repo_url} mono />
          <Field icon={FolderTree} label="Root" value={project.root} mono />
        </dl>
        <Separator className="my-4" />
        <div className="flex flex-wrap gap-1.5">
          {stages.length === 0 ? (
            <span className="text-xs text-muted-foreground">No tasks tracked.</span>
          ) : (
            stages.map((s) => (
              <Badge key={s} variant={stageVariant(s)} className="capitalize">
                {grouped.get(s)!.length} {lifecycleStageLabel(s)}
              </Badge>
            ))
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function Field({
  icon: Icon,
  label,
  value,
  mono,
}: {
  icon: typeof GitBranch;
  label: string;
  value: string | null | undefined;
  mono?: boolean;
}) {
  return (
    <div className="min-w-0">
      <dt className="flex items-center gap-1.5 text-[10px] uppercase tracking-wide text-muted-foreground">
        <Icon className="size-3" />
        {label}
      </dt>
      <dd
        className={cn(
          'mt-1 min-w-0 [overflow-wrap:anywhere] text-sm leading-snug',
          mono && 'font-mono',
        )}
      >
        {value || '—'}
      </dd>
    </div>
  );
}
