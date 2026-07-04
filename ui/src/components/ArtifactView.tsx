import { useCallback, useState } from 'react';
import { useNavigate, useParams, useSearch } from '@tanstack/react-router';
import { Loader2, RefreshCw } from 'lucide-react';
import { toast } from 'sonner';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { useEventStream } from '@/hooks/useEventStream';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchArtifact, regenerateArtifact } from '@/lib/api';
import { ArtifactRenderer } from '@/lib/artifacts/ArtifactRenderer';
import { routeSearch } from '@/lib/searchState';
import { useResource } from '@/lib/useResource';

import { ErrorPanel, PageHeader } from './Primitives';

const STATE_VARIANT: Record<string, 'outline' | 'default' | 'destructive' | 'secondary'> = {
  submitted: 'secondary',
  regenerating: 'default',
  failed: 'destructive',
};

export function ArtifactView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const { artifactId } = useParams({ strict: false }) as { artifactId: string };
  const search = useSearch({ strict: false }) as { version?: number };
  const refresh = useRefreshToken();
  const [regenerating, setRegenerating] = useState(false);

  const artifact = useResource(
    `artifact:${projectId}:${artifactId}:${search.version ?? 'latest'}:${refresh}`,
    () => fetchArtifact(artifactId, projectId, search.version),
  );

  useEventStream(
    useCallback(
      (event) => {
        if (event.topic === 'artifact') artifact.refresh();
      },
      // eslint-disable-next-line react-hooks/exhaustive-deps
      [],
    ),
  );

  function setVersion(version: number | undefined) {
    void navigate({
      search: routeSearch((prev) => ({ ...prev, version })),
    });
  }

  async function regenerate() {
    setRegenerating(true);
    try {
      await regenerateArtifact(artifactId, {}, projectId);
      toast.success('Regenerate started');
      artifact.refresh();
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setRegenerating(false);
    }
  }

  if (artifact.error) return <ErrorPanel error={artifact.error} />;
  if (!artifact.data) {
    return (
      <div className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="size-4 animate-spin" /> Loading artifact…
      </div>
    );
  }

  const data = artifact.data;
  const isArchivedVersion = typeof search.version === 'number' && search.version !== data.version;

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title={data.title || data.id}
        description={data.prompt}
        actions={
          <div className="flex items-center gap-2">
            <Select
              value={String(search.version ?? data.version)}
              onValueChange={(value) => setVersion(value === String(data.version) ? undefined : Number(value))}
            >
              <SelectTrigger className="w-28" size="sm">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {Array.from({ length: data.version }, (_, index) => data.version - index).map((version) => (
                  <SelectItem key={version} value={String(version)}>
                    v{version}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button
              type="button"
              variant="outline"
              size="sm"
              disabled={regenerating || isArchivedVersion}
              onClick={() => void regenerate()}
            >
              {regenerating ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
              Regenerate
            </Button>
          </div>
        }
      />
      <div className="flex flex-wrap items-center gap-1.5">
        <Badge variant={STATE_VARIANT[data.state] ?? 'outline'} className={data.state === 'regenerating' ? 'animate-pulse' : undefined}>
          {data.state}
        </Badge>
        {isArchivedVersion ? <Badge variant="outline">archived version {search.version}</Badge> : null}
        {data.subject_nodes.length === 0 ? <Badge variant="secondary">prompt-only</Badge> : null}
      </div>
      <div className="rounded-xl border bg-card/40 p-4">
        <ArtifactRenderer content={data.content} />
      </div>
    </div>
  );
}
