import { useMemo } from 'react';
import { useNavigate } from '@tanstack/react-router';

import { Badge } from '@/components/ui/badge';
import { useMe } from '@/hooks/useMe';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchArchitecture, fetchArtifacts, fetchDecisions, fetchGlossary } from '@/lib/api';
import type { ArchitectureSummary, ArtifactSummary, DecisionSummary, GlossarySummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';
import { inferNodeKind } from '@/components/node-views/orgNodes';

import { ErrorPanel, PageHeader } from './Primitives';
import { NodeListView } from './node-views/NodeListView';

const ARTIFACT_STATE_VARIANT: Record<string, 'outline' | 'default' | 'destructive' | 'secondary'> = {
  submitted: 'secondary',
  regenerating: 'default',
  failed: 'destructive',
};

function ArtifactStateBadge({ state }: { state: string }) {
  const variant = ARTIFACT_STATE_VARIANT[state] ?? 'outline';
  return (
    <Badge variant={variant} className={state === 'regenerating' ? 'animate-pulse' : undefined}>
      {state}
    </Badge>
  );
}

type SubjectDirectory = {
  decisions: DecisionSummary[];
  architecture: ArchitectureSummary[];
  glossary: GlossarySummary[];
};

function subjectLabel(id: string, dir: SubjectDirectory): string {
  const kind = inferNodeKind(id);
  if (kind === 'decision') return dir.decisions.find((d) => d.id === id)?.title ?? id;
  if (kind === 'architecture') return dir.architecture.find((a) => a.id === id)?.label ?? id;
  return dir.glossary.find((g) => g.id === id)?.canonical ?? id;
}

function subjectSummary(nodes: string[], dir: SubjectDirectory): string {
  if (nodes.length === 0) return 'Prompt-only';
  const labels = nodes.slice(0, 2).map((id) => subjectLabel(id, dir));
  const extra = nodes.length - labels.length;
  return extra > 0 ? `${labels.join(', ')} +${extra} more` : labels.join(', ');
}

export function ArtifactsView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const refresh = useRefreshToken();
  // Subject labels come from the graph nodes an artifact is "about"; a member
  // without graph.read (e.g. an artifacts-only reviewer) can't read those and
  // would 403 — skip the lookups and fall back to raw ids / "Prompt-only".
  const { can } = useMe();
  const canReadGraph = can(projectId, 'graph.read');
  const artifacts = useResource(`artifacts:${projectId}:${refresh}`, () => fetchArtifacts(projectId));
  const decisions = useResource(
    `artifacts-subjects-decisions:${projectId}:${refresh}`,
    () => fetchDecisions(projectId),
    { enabled: canReadGraph },
  );
  const architecture = useResource(
    `artifacts-subjects-architecture:${projectId}:${refresh}`,
    () => fetchArchitecture(projectId),
    { enabled: canReadGraph },
  );
  const glossary = useResource(
    `artifacts-subjects-glossary:${projectId}:${refresh}`,
    () => fetchGlossary(projectId),
    { enabled: canReadGraph },
  );

  const dir = useMemo<SubjectDirectory>(
    () => ({
      decisions: decisions.data ?? [],
      architecture: architecture.data ?? [],
      glossary: glossary.data ?? [],
    }),
    [architecture.data, decisions.data, glossary.data],
  );

  function openArtifact(id: string) {
    void navigate({ to: '/projects/$projectId/artifacts/$artifactId', params: { projectId, artifactId: id } });
  }

  if (artifacts.error) return <ErrorPanel error={artifacts.error} />;

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Artifacts"
        count={artifacts.data?.length}
        description={`Generated artifacts for ${projectId}.`}
      />
      <NodeListView<ArtifactSummary>
        ariaLabel="Artifacts"
        items={artifacts.data ?? []}
        getId={(item) => item.id}
        onSelect={openArtifact}
        loading={artifacts.loading}
        emptyLabel="No artifacts yet. Generate one from a section page or a node's aside."
        renderRow={(artifact) => (
          <div className="grid w-full gap-2 md:grid-cols-[1fr_auto] md:items-center">
            <div className="min-w-0">
              <p className="truncate text-sm font-medium">{artifact.title || artifact.id}</p>
              <p className="truncate text-xs text-muted-foreground">{subjectSummary(artifact.subject_nodes, dir)}</p>
            </div>
            <div className="flex flex-wrap items-center gap-1.5 md:justify-end">
              <ArtifactStateBadge state={artifact.state} />
              <Badge variant="outline" className="font-mono">
                v{artifact.version}
              </Badge>
              {artifact.open_comment_count > 0 ? (
                <Badge variant="secondary">{artifact.open_comment_count} open comment{artifact.open_comment_count === 1 ? '' : 's'}</Badge>
              ) : null}
            </div>
          </div>
        )}
      />
    </div>
  );
}
