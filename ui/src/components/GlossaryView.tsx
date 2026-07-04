import { useEffect, useMemo, useState } from 'react';
import { useNavigate, useSearch } from '@tanstack/react-router';
import { Sparkles } from 'lucide-react';

import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Checkbox } from '@/components/ui/checkbox';
import { useRefreshToken } from '@/hooks/useRefreshBus';
import { fetchGlossary } from '@/lib/api';
import { appendDrawerStack, routeSearch, type AppSearch } from '@/lib/searchState';
import type { GlossarySummary } from '@/lib/types';
import { useResource } from '@/lib/useResource';

import { GenerateArtifactDialog } from './GenerateArtifactDialog';
import { ErrorPanel, PageHeader } from './Primitives';
import { NodeListView } from './node-views/NodeListView';
import { NodeModal } from './node-views/NodeModal';
import { firstSentence } from './node-views/orgNodes';

type GlossarySearch = AppSearch & {
  q?: string;
};

export function GlossaryView({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as GlossarySearch;
  const refresh = useRefreshToken();
  const glossary = useResource(`glossary:${projectId}:${refresh}`, () => fetchGlossary(projectId));
  const query = search.q ?? '';
  const [selectMode, setSelectMode] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [generateOpen, setGenerateOpen] = useState(false);
  const [generateSelectionOpen, setGenerateSelectionOpen] = useState(false);

  // Drop selections that no longer exist once a live refresh lands, so a
  // stale id never rides along in a "Generate from N selected" request.
  useEffect(() => {
    if (!glossary.data) return;
    const known = new Set(glossary.data.map((item) => item.id));
    setSelected((current) => {
      const next = new Set([...current].filter((id) => known.has(id)));
      return next.size === current.size ? current : next;
    });
  }, [glossary.data]);

  const items = useMemo(() => {
    const q = query.trim().toLowerCase();
    return [...(glossary.data ?? [])]
      .sort((a, b) => (a.canonical ?? a.id).localeCompare(b.canonical ?? b.id))
      .filter((term) => {
        if (!q) return true;
        return `${term.canonical ?? term.id} ${term.definition ?? ''}`.toLowerCase().includes(q);
      });
  }, [glossary.data, query]);

  function setQuery(value: string) {
    void navigate({
      search: routeSearch((prev) => ({
        ...prev,
        q: value || undefined,
      })),
      replace: true,
    });
  }

  function openNode(id: string) {
    void navigate({
      search: routeSearch((prev) => appendDrawerStack(prev, id)),
    });
  }

  function toggleSelected(id: string) {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  if (glossary.error) return <ErrorPanel error={glossary.error} />;

  return (
    <div className="flex flex-col gap-4">
      <PageHeader
        title="Glossary"
        count={items.length}
        description="Canonical domain language. IDs stay hidden on this page."
        actions={
          <>
            <Button
              type="button"
              variant={selectMode ? 'default' : 'outline'}
              size="sm"
              aria-pressed={selectMode}
              onClick={() => {
                setSelectMode((v) => !v);
                setSelected(new Set());
              }}
            >
              {selectMode ? `${selected.size} selected` : 'Select'}
            </Button>
            {selectMode ? (
              <Button type="button" size="sm" disabled={selected.size === 0} onClick={() => setGenerateSelectionOpen(true)}>
                <Sparkles />
                Generate from {selected.size} selected
              </Button>
            ) : (
              <Button type="button" size="sm" onClick={() => setGenerateOpen(true)}>
                <Sparkles />
                Generate artifact
              </Button>
            )}
          </>
        }
      />
      <NodeListView<GlossarySummary>
        ariaLabel="Glossary"
        items={items}
        getId={(item) => item.id}
        search={query}
        onSearchChange={setQuery}
        onSelect={selectMode ? toggleSelected : openNode}
        loading={glossary.loading}
        renderRow={(term) => (
          <div className="grid w-full gap-2 md:grid-cols-[1fr_auto] md:items-center">
            <div className="min-w-0">
              <p className="truncate text-sm font-semibold">{term.canonical ?? term.id}</p>
              <p className="truncate text-xs text-muted-foreground">{firstSentence(term.definition)}</p>
            </div>
            <div className="flex flex-wrap gap-1.5 md:justify-end">
              <Badge variant="outline" className="font-mono">↔{term.relates_to.length}</Badge>
            </div>
          </div>
        )}
        renderActionZone={
          selectMode
            ? (term) => (
                <Checkbox
                  checked={selected.has(term.id)}
                  onCheckedChange={() => toggleSelected(term.id)}
                  aria-label={`Select ${term.canonical ?? term.id}`}
                />
              )
            : undefined
        }
      />
      <GenerateArtifactDialog
        projectId={projectId}
        open={generateOpen}
        onOpenChange={setGenerateOpen}
        nodes={(glossary.data ?? []).map((item) => item.id)}
        nodeLabels={(glossary.data ?? []).map((item) => item.canonical ?? item.id)}
      />
      <GenerateArtifactDialog
        projectId={projectId}
        open={generateSelectionOpen}
        onOpenChange={(next) => {
          setGenerateSelectionOpen(next);
          if (!next) setSelectMode(false);
        }}
        nodes={[...selected]}
        nodeLabels={[...selected].map((id) => (glossary.data ?? []).find((item) => item.id === id)?.canonical ?? id)}
      />
      <NodeModal
        projectId={projectId}
        nodeKind="glossary"
        seed={{ glossary: glossary.data ?? null }}
      />
    </div>
  );
}
