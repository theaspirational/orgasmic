// @arch arch_MK2Q2.7
import { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { ArrowLeft, Check, Copy, ExternalLink, Eye, Pencil } from 'lucide-react';

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
import { architectureDescriptorFor, DESCRIPTORS } from '@/components/orgdoc/descriptor';
import { NodeDocEditor, type NodeDirectory } from '@/components/orgdoc/NodeDocEditor';
import { useIsMobile } from '@/hooks/use-mobile';
import { fetchArchitecture, fetchDecisions, fetchGlossary } from '@/lib/api';
import { appendDrawerStack, routeSearch, searchList, withDrawerStack, type AppSearch } from '@/lib/searchState';
import type { ArchitectureSummary, DecisionSummary, GlossarySummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { CopyIdBadge } from '../CopyIdBadge';
import { inferNodeKind, shortPath, type NodeKind } from './orgNodes';

// Summaries for every layer, so the modal can resolve cross-kind chip labels,
// autocomplete, and breadcrumb titles while the per-node editor fetches its own
// structured document. No client-side `.org` parsing happens here anymore.
type DetailData = {
  decisions: DecisionSummary[];
  architecture: ArchitectureSummary[];
  glossary: GlossarySummary[];
};

type DetailSeed = Partial<{
  decisions: DecisionSummary[] | null;
  architecture: ArchitectureSummary[] | null;
  glossary: GlossarySummary[] | null;
}>;

async function loadDetailData(projectId: string, seed: DetailSeed = {}): Promise<DetailData> {
  const [decisions, architecture, glossary] = await Promise.all([
    seed.decisions ? Promise.resolve(seed.decisions) : fetchDecisions(projectId),
    seed.architecture ? Promise.resolve(seed.architecture) : fetchArchitecture(projectId),
    seed.glossary ? Promise.resolve(seed.glossary) : fetchGlossary(projectId),
  ]);
  return { decisions, architecture, glossary };
}

function nodeTitle(kind: NodeKind, id: string, data: DetailData): string {
  if (kind === 'decision') return data.decisions.find((d) => d.id === id)?.title || id;
  if (kind === 'architecture') return data.architecture.find((a) => a.id === id)?.label || id;
  return data.glossary.find((t) => t.id === id)?.canonical || id;
}

function buildDirectory(data: DetailData | null): NodeDirectory {
  const decisions = data?.decisions ?? [];
  const architecture = data?.architecture ?? [];
  const glossary = data?.glossary ?? [];
  return {
    labelFor: (id) => {
      if (id.startsWith('dec_')) return decisions.find((d) => d.id === id)?.title ?? id;
      if (id.startsWith('arch_')) return architecture.find((a) => a.id === id)?.label ?? id;
      return glossary.find((t) => t.id === id)?.canonical ?? id;
    },
    suggestionsFor: (source) => {
      if (source === 'decision') return decisions.map((d) => ({ value: d.id, label: d.title }));
      if (source === 'architecture') return architecture.map((a) => ({ value: a.id, label: a.label }));
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
  const stack = useMemo(() => searchList(search.drawer_stack), [search.drawer_stack]);
  const activeId = stack.at(-1) ?? null;
  const activeKind = inferNodeKind(activeId) ?? nodeKind;
  const open = stack.length > 0;
  const detail = useResource(
    `node-modal:${projectId}`,
    () => loadDetailData(projectId, seed),
    { enabled: open },
  );
  const trail = stack;
  const [mode, setMode] = useState<'view' | 'edit'>('view');

  useEffect(() => {
    setMode('view');
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

  const content = (
    <NodeModalContent
      projectId={projectId}
      activeId={activeId}
      activeKind={activeKind}
      data={detail.data}
      loading={detail.loading && !detail.data}
      error={detail.error}
      breadcrumbs={trail}
      mode={mode}
      onBack={popFrame}
      onPopTo={popToTrailIndex}
      onOpenNode={pushNode}
      onToggleMode={() => setMode((current) => (current === 'edit' ? 'view' : 'edit'))}
    />
  );

  if (isMobile) {
    return (
      <Sheet open={open} onOpenChange={(next) => !next && popFrame()}>
        <SheetContent side="right" className="w-full p-0 sm:max-w-none md:max-w-[44rem]">
          <SheetHeader className="border-b pr-12">
            <SheetTitle>{title}</SheetTitle>
            <SheetDescription>{description}</SheetDescription>
          </SheetHeader>
          {content}
        </SheetContent>
      </Sheet>
    );
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !next && popFrame()}>
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
}) {
  const directory = useMemo<NodeDirectory>(() => buildDirectory(data), [data]);

  if (loading) {
    return (
      <div className="flex flex-col gap-3 p-5">
        <Skeleton className="h-6 w-48" />
        <Skeleton className="h-64" />
      </div>
    );
  }
  if (error) {
    return (
      <div className="p-5 text-sm text-destructive">
        {error instanceof Error ? error.message : String(error)}
      </div>
    );
  }
  if (!activeId || !data) return null;
  const title = nodeTitle(activeKind, activeId, data);
  const hiddenStackCount = Math.max(0, breadcrumbs.length - 1);

  return (
    <>
      <div className="flex items-start gap-3 border-b px-5 py-4 pr-12">
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          className="hidden sm:inline-flex"
          onClick={onBack}
          aria-label="Back"
        >
          <ArrowLeft />
        </Button>
        <Button type="button" variant="ghost" size="sm" className="sm:hidden" onClick={onBack}>
          <ArrowLeft />
          Back{hiddenStackCount > 0 ? ` (${hiddenStackCount} more)` : ''}
        </Button>
        <div className="min-w-0 flex-1">
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
          <h2 className="mt-1 text-base font-semibold leading-snug">{title}</h2>
        </div>
        <Button
          type="button"
          variant={mode === 'edit' ? 'default' : 'outline'}
          size="sm"
          className="shrink-0"
          onClick={onToggleMode}
          aria-pressed={mode === 'edit'}
        >
          {mode === 'edit' ? <Eye /> : <Pencil />}
          {mode === 'edit' ? 'View' : 'Edit'}
        </Button>
      </div>
      <ScrollArea className="min-h-0">
        <div className="grid gap-5 p-5 md:grid-cols-[1fr_16rem]">
          <div className="min-w-0">
            <NodeDocEditor
              projectId={projectId}
              nodeId={activeId}
              descriptor={
                activeKind === 'architecture'
                  ? architectureDescriptorFor(activeId)
                  : DESCRIPTORS[activeKind]
              }
              directory={directory}
              onOpenNode={onOpenNode}
              mode={mode}
            />
          </div>
          <Aside id={activeId} kind={activeKind} data={data} />
        </div>
      </ScrollArea>
    </>
  );
}

function Aside({
  id,
  kind,
  data,
}: {
  id: string;
  kind: NodeKind;
  data: DetailData;
}) {
  const archNode = kind === 'architecture' ? data.architecture.find((item) => item.id === id) : undefined;
  const source = kind === 'decision'
    ? data.decisions.find((item) => item.id === id)?.source_file
    : kind === 'architecture'
      ? archNode?.source_file
      : data.glossary.find((item) => item.id === id)?.source_file;
  const tests = archNode?.tests ?? [];
  return (
    <aside className="flex min-w-0 flex-col gap-3 rounded-md border bg-muted/20 p-3">
      <div>
        <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">Source</dt>
        <dd className="mt-1 flex items-center gap-1 font-mono text-xs">
          {shortPath(source)}
          {source ? <ExternalLink className="size-3 text-muted-foreground" /> : null}
        </dd>
      </div>
      <Separator />
      <div>
        <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">Kind</dt>
        <dd className="mt-1">
          <Badge variant="outline">{kind}</Badge>
        </dd>
      </div>
      {tests.length > 0 ? <NodeTests tests={tests} /> : null}
    </aside>
  );
}

// Per-node test commands (:TESTS:) — the scoped suite an agent should run when
// touching this node's source paths, instead of the whole workspace.
function NodeTests({ tests }: { tests: string[] }) {
  const [copied, setCopied] = useState<number | null>(null);
  const copy = useCallback((cmd: string, index: number) => {
    void navigator.clipboard?.writeText(cmd).then(() => {
      setCopied(index);
      window.setTimeout(() => setCopied((current) => (current === index ? null : current)), 1200);
    });
  }, []);
  return (
    <>
      <Separator />
      <div>
        <dt className="text-[10px] uppercase tracking-wide text-muted-foreground">Tests</dt>
        <dd className="mt-1 flex flex-col gap-1">
          {tests.map((cmd, index) => (
            <button
              key={cmd}
              type="button"
              onClick={() => copy(cmd, index)}
              title="Copy command"
              className="group flex items-center justify-between gap-1 rounded border bg-background px-1.5 py-1 text-left font-mono text-[11px] leading-tight hover:border-foreground/30"
            >
              <span className="min-w-0 truncate">{cmd}</span>
              {copied === index ? (
                <Check className="size-3 shrink-0 text-muted-foreground" />
              ) : (
                <Copy className="size-3 shrink-0 text-muted-foreground opacity-0 group-hover:opacity-100" />
              )}
            </button>
          ))}
        </dd>
      </div>
    </>
  );
}
