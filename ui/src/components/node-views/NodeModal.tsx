import { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { ExternalLink, Eye, Pencil, Sparkles } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Dialog, DialogContent, DialogDescription, DialogTitle } from '@/components/ui/dialog';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Separator } from '@/components/ui/separator';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';
import { Skeleton } from '@/components/ui/skeleton';
import { DESCRIPTORS } from '@/components/orgdoc/descriptor';
import { NodeDocEditor, type NodeDirectory } from '@/components/orgdoc/NodeDocEditor';
import { useIsMobile } from '@/hooks/use-mobile';
import { useMe } from '@/hooks/useMe';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchDecisions, fetchGlossary } from '@/lib/api';
import type { OrgNodeDoc } from '@/lib/orgdoc/types';
import { appendDrawerStack, routeSearch, searchList, withDrawerStack, type AppSearch } from '@/lib/searchState';
import type { DecisionSummary, GlossarySummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { CopyIdBadge } from '../CopyIdBadge';
import { GenerateArtifactDialog } from '../GenerateArtifactDialog';
import { PeekBackButton } from '../PeekBackButton';
import { inferNodeKind, shortPath, type NodeKind } from './orgNodes';
import { NodeDeleteControl } from './NodeDeleteControl';

// Summaries for every editable node kind, so the modal can resolve cross-kind chip labels,
// autocomplete, and breadcrumb titles while the per-node editor fetches its own
// structured document. No client-side `.org` parsing happens here anymore.
type DetailData = {
  decisions: DecisionSummary[];
  glossary: GlossarySummary[];
};

type DetailSeed = Partial<{
  decisions: DecisionSummary[] | null;
  glossary: GlossarySummary[] | null;
}>;

function activeSeedVersion(seed: DetailSeed, activeKind: NodeKind, activeId: string | null): string {
  if (!activeId) return 'none';
  if (activeKind === 'decision') {
    if (!seed.decisions) return 'fetch';
    const decision = seed.decisions.find((item) => item.id === activeId);
    return decision
      ? [
          decision.id,
          decision.parent ?? '',
          decision.path ?? '',
          (decision.children ?? []).join(','),
          decision.title,
          decision.preview ?? '',
        ].join(':')
      : 'missing';
  }
  if (!seed.glossary) return 'fetch';
  const glossary = seed.glossary.find((item) => item.id === activeId);
  return glossary ? [glossary.id, glossary.canonical ?? ''].join(':') : 'missing';
}

function seedHasActiveNode(seed: DetailSeed, activeKind: NodeKind, activeId: string | null): boolean {
  if (!activeId) return true;
  if (activeKind === 'decision') return Boolean(seed.decisions?.some((item) => item.id === activeId));
  return Boolean(seed.glossary?.some((item) => item.id === activeId));
}

function detailHasActiveNode(data: DetailData | null, activeKind: NodeKind, activeId: string | null): boolean {
  if (!data || !activeId) return false;
  if (activeKind === 'decision') return data.decisions.some((item) => item.id === activeId);
  return data.glossary.some((item) => item.id === activeId);
}

async function loadDetailData(
  projectId: string,
  seed: DetailSeed = {},
  activeKind: NodeKind,
  activeId: string | null,
): Promise<DetailData> {
  const activeSeedIsFreshEnough = seedHasActiveNode(seed, activeKind, activeId);
  const [decisions, glossary] = await Promise.all([
    seed.decisions && (activeKind !== 'decision' || activeSeedIsFreshEnough)
      ? Promise.resolve(seed.decisions)
      : fetchDecisions(projectId),
    seed.glossary && (activeKind !== 'glossary' || activeSeedIsFreshEnough)
      ? Promise.resolve(seed.glossary)
      : fetchGlossary(projectId),
  ]);
  return { decisions, glossary };
}

function nodeTitle(kind: NodeKind, id: string, data: DetailData): string {
  if (kind === 'decision') return data.decisions.find((d) => d.id === id)?.title || id;
  return data.glossary.find((t) => t.id === id)?.canonical || id;
}

function decisionParentTrail(id: string, decisions: DecisionSummary[]): DecisionSummary[] {
  const byId = new Map(decisions.map((decision) => [decision.id, decision]));
  const out: DecisionSummary[] = [];
  const seen = new Set<string>();
  let current = byId.get(id)?.parent ?? null;
  while (current && !seen.has(current)) {
    seen.add(current);
    const parent = byId.get(current);
    if (!parent) break;
    out.push(parent);
    current = parent.parent ?? null;
  }
  return out.reverse();
}

function buildDirectory(data: DetailData | null): NodeDirectory {
  const decisions = data?.decisions ?? [];
  const glossary = data?.glossary ?? [];
  return {
    labelFor: (id) => {
      if (id.startsWith('dec_')) return decisions.find((d) => d.id === id)?.title ?? id;
      return glossary.find((t) => t.id === id)?.canonical ?? id;
    },
    suggestionsFor: (source) => {
      if (source === 'decision') return decisions.map((d) => ({ value: d.id, label: d.title }));
      return glossary.map((t) => ({ value: t.id, label: t.canonical ?? t.id }));
    },
  };
}

export function NodeModal({
  projectId,
  nodeKind,
  seed = {},
}: {
  projectId: string;
  nodeKind: NodeKind;
  seed?: DetailSeed;
}) {
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as AppSearch & { drawer_stack?: string[] };
  const isMobile = useIsMobile();
  const refresh = useRefreshToken();
  const stack = useMemo(() => searchList(search.drawer_stack), [search.drawer_stack]);
  const activeId = stack.at(-1) ?? null;
  const activeKind = inferNodeKind(activeId) ?? nodeKind;
  const open = stack.length > 0;
  const seedVersion = activeSeedVersion(seed, activeKind, activeId);
  const detail = useResource(
    `node-modal:${projectId}:${activeKind}:${activeId ?? 'closed'}:${refresh}:${seedVersion}`,
    () => loadDetailData(projectId, seed, activeKind, activeId),
    { enabled: open },
  );
  const trail = stack;
  const [mode, setMode] = useState<'view' | 'edit'>('view');
  const [activeDocument, setActiveDocument] = useState<OrgNodeDoc | null>(null);

  useEffect(() => {
    setMode('view');
    setActiveDocument(null);
  }, [activeId]);

  const pushNode = useCallback((id: string) => {
    void navigate({
      search: routeSearch((prev) => appendDrawerStack(prev, id)),
    });
  }, [navigate]);

  const closeRoute = useCallback(() => {
    void navigate({
      search: routeSearch((prev) => withDrawerStack(prev, [])),
      replace: true,
    });
  }, [navigate]);

  const popFrame = useCallback(() => {
    if (stack.length > 0) {
      void navigate({
        search: routeSearch((prev) => withDrawerStack(prev, stack.slice(0, -1))),
      });
      return;
    }
    closeRoute();
  }, [closeRoute, navigate, stack]);

  const popToTrailIndex = useCallback((index: number) => {
    if (index >= trail.length - 1) return;
    void navigate({
      search: routeSearch((prev) => withDrawerStack(prev, stack.slice(0, index + 1))),
    });
  }, [navigate, stack, trail.length]);

  const title = useMemo(() => {
    if (!activeId) return 'Node';
    if (!detail.data) return activeKind === 'glossary' ? 'Glossary term' : activeId;
    return nodeTitle(activeKind, activeId, detail.data);
  }, [activeId, activeKind, detail.data]);
  const description = activeKind === 'glossary' ? title : (activeId ?? 'Node');
  const waitingForActiveSummary = Boolean(
    activeId && detail.data && !detailHasActiveNode(detail.data, activeKind, activeId),
  );

  const content = (
    <NodeModalContent
      projectId={projectId}
      activeId={activeId}
      activeKind={activeKind}
      data={detail.data}
      loading={detail.loading && (!detail.data || waitingForActiveSummary)}
      error={detail.error}
      breadcrumbs={trail}
      mode={mode}
      onBack={popFrame}
      onPopTo={popToTrailIndex}
      onOpenNode={pushNode}
      onToggleMode={() => setMode((current) => (current === 'edit' ? 'view' : 'edit'))}
      activeDocument={activeDocument}
      onDocumentChange={setActiveDocument}
      onDeleted={closeRoute}
    />
  );

  if (isMobile) {
    return (
      <Sheet open={open} onOpenChange={(next) => !next && closeRoute()}>
        <SheetContent
          side="right"
          className="gap-0 p-0 data-[side=right]:w-full sm:max-w-none md:max-w-[44rem]"
        >
          <SheetHeader className="sr-only">
            <SheetTitle>{title}</SheetTitle>
            <SheetDescription>{description}</SheetDescription>
          </SheetHeader>
          {content}
        </SheetContent>
      </Sheet>
    );
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !next && closeRoute()}>
      <DialogContent className="grid h-[min(90vh,46rem)] w-[min(96vw,72rem)] max-w-none grid-rows-[auto_1fr] gap-0 overflow-hidden p-0 sm:max-w-none">
        <DialogTitle className="sr-only">{title}</DialogTitle>
        <DialogDescription className="sr-only">{description}</DialogDescription>
        {content}
      </DialogContent>
    </Dialog>
  );
}

function NodeModalContent({
  projectId,
  activeId,
  activeKind,
  data,
  loading,
  error,
  breadcrumbs,
  mode,
  onBack,
  onPopTo,
  onOpenNode,
  onToggleMode,
  activeDocument,
  onDocumentChange,
  onDeleted,
}: {
  projectId: string;
  activeId: string | null;
  activeKind: NodeKind;
  data: DetailData | null;
  loading: boolean;
  error: unknown | null;
  breadcrumbs: string[];
  mode: 'view' | 'edit';
  onBack: () => void;
  onPopTo: (index: number) => void;
  onOpenNode: (id: string) => void;
  onToggleMode: () => void;
  activeDocument: OrgNodeDoc | null;
  onDocumentChange: (document: OrgNodeDoc | null) => void;
  onDeleted: () => void;
}) {
  const directory = useMemo<NodeDirectory>(() => buildDirectory(data), [data]);
  const { isMember } = useMe();

  if (loading) {
    return (
      <>
        <div className="flex items-center gap-3 border-b px-5 py-4 pr-12">
          <PeekBackButton depth={breadcrumbs.length} onBack={onBack} />
          <Skeleton className="h-6 w-48" />
        </div>
        <div className="p-5">
          <Skeleton className="h-64" />
        </div>
      </>
    );
  }
  if (error) {
    return (
      <>
        <div className="flex items-center gap-3 border-b px-5 py-4 pr-12">
          <PeekBackButton depth={breadcrumbs.length} onBack={onBack} />
          <h2 className="text-base font-semibold">Unable to load item</h2>
        </div>
        <div className="p-5 text-sm text-destructive">
          {error instanceof Error ? error.message : String(error)}
        </div>
      </>
    );
  }
  if (!activeId || !data) return null;
  const title = nodeTitle(activeKind, activeId, data);
  const parentTrail = activeKind === 'decision' ? decisionParentTrail(activeId, data.decisions) : [];
  const activeDecision = activeKind === 'decision'
    ? data.decisions.find((decision) => decision.id === activeId)
    : undefined;
  const baseVersion = activeDocument?.id === activeId
    ? activeDocument.source.base_version
    : null;

  return (
    <>
      <div className="flex flex-wrap items-start gap-3 border-b px-5 py-4 pr-12 sm:flex-nowrap">
        <PeekBackButton depth={breadcrumbs.length} onBack={onBack} />
        <div className="order-3 min-w-0 w-full sm:order-none sm:w-auto sm:flex-1">
          {breadcrumbs.length > 1 ? (
            <nav className="mb-2 hidden flex-wrap items-center gap-1 text-xs text-muted-foreground sm:flex" aria-label="Drawer stack">
              {breadcrumbs.map((id, index) => (
                <span key={`${id}:${index}`} className="inline-flex items-center gap-1">
                  {index > 0 ? <span aria-hidden="true">&gt;</span> : null}
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-6 px-1.5 font-mono text-xs text-muted-foreground"
                    disabled={index === breadcrumbs.length - 1}
                    onClick={() => onPopTo(index)}
                  >
                    {nodeTitle(inferNodeKind(id) ?? activeKind, id, data)}
                  </Button>
                </span>
              ))}
            </nav>
          ) : null}
          {activeKind !== 'glossary' && activeId ? (
            <CopyIdBadge value={activeId} className="h-4 px-1.5 text-[9px]" />
          ) : null}
          {parentTrail.length > 0 ? (
            <nav className="mt-2 flex flex-wrap items-center gap-1 text-xs text-muted-foreground" aria-label="Decision parent breadcrumb">
              <span>Parent</span>
              {parentTrail.map((decision, index) => (
                <span key={decision.id} className="inline-flex items-center gap-1">
                  <span aria-hidden="true">{index === 0 ? ':' : '>'}</span>
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    className="h-6 px-1.5 text-xs text-muted-foreground"
                    onClick={() => onOpenNode(decision.id)}
                  >
                    {decision.path ? `${decision.path} ` : ''}{decision.title || decision.id}
                  </Button>
                </span>
              ))}
            </nav>
          ) : null}
          <h2 className="mt-1 text-base font-semibold leading-snug">{title}</h2>
        </div>
        <div className="order-2 ml-auto flex shrink-0 items-center gap-1 sm:order-none sm:ml-0">
          <Button
            type="button"
            variant={mode === 'edit' ? 'default' : 'outline'}
            size="sm"
            onClick={onToggleMode}
            aria-pressed={mode === 'edit'}
          >
            {mode === 'edit' ? <Eye data-icon="inline-start" /> : <Pencil data-icon="inline-start" />}
            {mode === 'edit' ? 'View' : 'Edit'}
          </Button>
          {!isMember ? (
            <NodeDeleteControl
              projectId={projectId}
              id={activeId}
              kind={activeKind}
              title={title}
              baseVersion={baseVersion}
              childCount={activeDecision?.children?.length ?? 0}
              onDeleted={onDeleted}
            />
          ) : null}
        </div>
      </div>
      <ScrollArea className="min-h-0 flex-1">
        <div className="grid gap-5 p-5 md:grid-cols-[1fr_16rem]">
          <div className="min-w-0">
            <NodeDocEditor
              projectId={projectId}
              nodeId={activeId}
              descriptor={DESCRIPTORS[activeKind]}
              directory={directory}
              onOpenNode={onOpenNode}
              mode={mode}
              onDocumentChange={onDocumentChange}
            />
          </div>
          <Aside projectId={projectId} id={activeId} kind={activeKind} data={data} onOpenNode={onOpenNode} />
        </div>
      </ScrollArea>
    </>
  );
}

function Aside({
  projectId,
  id,
  kind,
  data,
  onOpenNode,
}: {
  projectId: string;
  id: string;
  kind: NodeKind;
  data: DetailData;
  onOpenNode: (id: string) => void;
}) {
  const [generateOpen, setGenerateOpen] = useState(false);
  const { can } = useMe();
  const canGenerate = can(projectId, 'artifacts.generate');
  const decision = kind === 'decision' ? data.decisions.find((item) => item.id === id) : undefined;
  const decisionChildren = decision
    ? (decision.children ?? [])
        .map((childId) => data.decisions.find((item) => item.id === childId))
        .filter((item): item is DecisionSummary => Boolean(item))
    : [];
  const source = kind === 'decision'
    ? data.decisions.find((item) => item.id === id)?.source_file
    : data.glossary.find((item) => item.id === id)?.source_file;
  const nodeLabel = nodeTitle(kind, id, data);
  return (
    <aside className="flex min-w-0 flex-col gap-3 rounded-md border bg-muted/20 p-3">
      {canGenerate ? (
        <>
          <Button type="button" variant="outline" size="sm" onClick={() => setGenerateOpen(true)}>
            <Sparkles />
            Generate artifact
          </Button>
          <GenerateArtifactDialog
            projectId={projectId}
            open={generateOpen}
            onOpenChange={setGenerateOpen}
            nodes={[id]}
            nodeLabels={[nodeLabel]}
          />
          <Separator />
        </>
      ) : null}
      <div>
        <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">Source</dt>
        <dd className="mt-1 flex items-center gap-1 font-mono text-xs">
          {shortPath(source)}
          {source ? <ExternalLink className="size-3 text-muted-foreground" /> : null}
        </dd>
      </div>
      <Separator />
      {decision ? (
        <>
          <div>
            <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">Decision path</dt>
            <dd className="mt-1 font-mono text-xs">{decision.path ?? '—'}</dd>
          </div>
          <Separator />
          <div>
            <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">
              Children{decisionChildren.length ? ` (${decisionChildren.length})` : ''}
            </dt>
            <dd className="mt-1 flex flex-col gap-1">
              {decisionChildren.length === 0 ? (
                <span className="text-xs text-muted-foreground">No child decisions.</span>
              ) : (
                decisionChildren.map((child) => (
                  <button
                    key={child.id}
                    type="button"
                    className="rounded border bg-background px-2 py-1 text-left text-xs hover:border-foreground/30"
                    onClick={() => onOpenNode(child.id)}
                  >
                    <span className="font-mono text-muted-foreground">{child.path ?? '—'}</span>{' '}
                    <span>{child.title || child.id}</span>
                  </button>
                ))
              )}
            </dd>
          </div>
          <Separator />
        </>
      ) : null}
      <div>
        <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">Kind</dt>
        <dd className="mt-1">
          <Badge variant="outline">{kind}</Badge>
        </dd>
      </div>
    </aside>
  );
}
